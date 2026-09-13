//! What the controller's screen does when someone taps something.
//!
//! Each case below is a bug that reached hardware. They are written against
//! [`ControllerView`] — the phone's screen — rather than against protocol
//! fields, because the screen is what was wrong every time: a spinner that
//! never stopped, a progress bar that blanked, a title one track behind.
//!
//! Every case ends in [`ControllerHarness::assert_invariants`]. That is
//! deliberate: the invariants are shared, so a new rule added there is enforced
//! by every case at once, and a case written for one bug catches the next one
//! for free.

use super::controller_harness::{
    ControllerHarness, EngineCall, ReportTick, Transport, BUFFER_STATE_BUFFERING,
};

/// A queue and a duration long enough that positions are unambiguous.
const TRACKS: [u64; 3] = [101, 102, 103];
/// `FakeEngine` reports every synthetic track as four minutes long.
const TRACK_DURATION_MS: u64 = 240_000;

/// Tapping a track in an album: the queue push and the SetState that follows it
/// must leave the screen PLAYING, on that track, with exactly one stream opened.
///
/// The "one stream" half is the load-dedup window (`LOAD_ATTEMPT_DEDUP_WINDOW`).
/// The queue push starts the selection, the cloud's SetState names the same
/// track a moment later, and a renderer that treats the second as a fresh load
/// restarts the track under the user — the "first-track hiccup on album change".
#[tokio::test]
async fn tapping_a_track_starts_it_once_and_settles_the_screen() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;

    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    let streams: Vec<_> = harness
        .engine_calls()
        .into_iter()
        .filter(|call| matches!(call, EngineCall::StartStream { .. }))
        .collect();
    assert_eq!(
        streams,
        vec![EngineCall::StartStream {
            track_id: 101,
            start_secs: 0
        }],
        "one tap must open exactly one stream; timeline:\n{}",
        harness.rendered_timeline()
    );

    harness.report_tick().await;
    assert_eq!(harness.view().transport, Transport::Playing);
    assert_eq!(harness.view().total_ms, Some(TRACK_DURATION_MS));
    harness.assert_invariants();
}

/// Pause, then play, must not blank the progress display.
///
/// The controller reads an explicit `duration: null` as "blank the progress
/// bar", so a renderer that answers a pause with a null duration flicks the
/// elapsed time and total length to `0:00` until its next periodic report. The
/// echo report now states the duration the renderer knows; this asserts it keeps
/// doing so.
#[tokio::test]
async fn pause_and_play_never_blank_the_progress_display() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    harness.advance_playback(30_000);
    harness.report_tick().await;
    assert_eq!(
        harness.view().total_ms,
        Some(TRACK_DURATION_MS),
        "the report tick should have published a track length"
    );

    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;
    harness.controller().tap_play().await;

    // The invariant is the real assertion — it fires on the transition, and
    // names the step that blanked. These two pin the end state as well.
    harness.assert_invariants();
    assert_eq!(harness.view().total_ms, Some(TRACK_DURATION_MS));
    assert_eq!(harness.view().elapsed_ms, Some(30_000));
}

/// A pause from the phone must not drag playback back to an earlier track.
///
/// The controller sends pause as state ONLY — no track, no position. A renderer
/// that falls back to the cloud's last-known `current_track` to decide what to
/// align and load will use a value that is behind the local advance, and the
/// pause lands as a track change. Observed as "pause from iOS made qbz jump back
/// to a previous track".
#[tokio::test]
async fn a_state_only_pause_does_not_rewind_to_an_earlier_track() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    // Move on to the second track, the way a next-tap does.
    harness.controller().tap_next_track(102, Some(103)).await;
    harness.advance_playback(12_000);
    harness.report_tick().await;

    let before = harness.engine_calls().len();
    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert_eq!(
        after,
        vec![EngineCall::Pause],
        "a state-only pause must pause and nothing else; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.engine().snapshot().track_id, 102);
    assert_eq!(harness.view().transport, Transport::Paused);
    harness.assert_invariants();
}

/// A pause within 1.5 s of a load is IGNORED, on purpose.
///
/// Claiming the render from a peer makes that peer stop its own playback, and
/// the cloud relays the result to us milliseconds after telling us to play —
/// honouring it killed the stream we had just started and left the controller
/// spinning. The window is the price: a user who taps pause immediately after a
/// track starts is not obeyed.
///
/// This is pinned as behaviour rather than left implicit because the window is
/// keyed on wall-clock `Instant`, so it is invisible to every other test here
/// unless they step out of it first.
#[tokio::test]
async fn a_pause_inside_the_handoff_window_is_ignored_as_a_peer_echo() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    // No `leave_handoff_echo_window` — the load just happened.
    harness.controller().tap_pause().await;

    assert!(
        !harness.engine_calls().contains(&EngineCall::Pause),
        "a pause inside the handoff window must not reach the player; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert!(
        harness.engine().snapshot().is_playing,
        "the stream we just started must survive the peer's handoff echo"
    );

    // And the very same gesture IS obeyed once the window has passed.
    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;
    assert!(harness.engine_calls().contains(&EngineCall::Pause));
    assert!(!harness.engine().snapshot().is_playing);
    harness.assert_invariants();
}

/// Dragging the progress bar on the phone must move the audio thread (#387).
///
/// A seek arrives as a SetState carrying ONLY `current_position` — no
/// playing_state, no track. An earlier echo filter matched exactly that shape
/// and swallowed it, so the cloud's progress bar advanced while the audio stayed
/// put and the controller's bar locked.
#[tokio::test]
async fn dragging_the_progress_bar_moves_the_player() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(10_000);
    harness.report_tick().await;

    harness.controller().drag_seek(90_000).await;

    assert!(
        harness
            .engine_calls()
            .contains(&EngineCall::Seek { position_secs: 90 }),
        "a position-only SetState is a seek, not an echo; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.engine().snapshot().position, 90);
    harness.assert_invariants();
}

/// The cloud echoes every state report back as a SetState. That echo is not an
/// instruction, and it must reach neither the wire nor the player.
///
/// Both halves matter and they fail separately. Answering the echo with a report
/// is the feedback loop — report, echo, report — that used to lock the session
/// up. ACTING on it is quieter and worse: the echo of a pause report pauses
/// again, the echo of a resume resumes over the user's pause, and none of it
/// shows up as extra traffic.
#[tokio::test]
async fn the_clouds_echo_of_our_own_report_reaches_neither_wire_nor_player() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(5_000);
    harness.report_tick().await;
    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;

    for step in harness.timeline() {
        assert!(
            step.echo_engine_calls.is_empty(),
            "'{}' acted on the cloud's echo — {:?}; timeline:\n{}",
            step.label,
            step.echo_engine_calls,
            harness.rendered_timeline()
        );
        assert!(
            step.reports.len() <= 2,
            "'{}' answered the cloud's echo — {} reports; timeline:\n{}",
            step.label,
            step.reports.len(),
            harness.rendered_timeline()
        );
    }
    harness.assert_invariants();
}

/// Moving the volume slider reaches the player and comes back as exactly one
/// report carrying the level the controller asked for.
#[tokio::test]
async fn the_controllers_volume_reaches_the_player_and_is_reported_once() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    harness.controller().set_volume(64).await;

    assert!(
        harness
            .engine_calls()
            .contains(&EngineCall::SetVolume { pct: 64 }),
        "timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.view().volume, Some(64));

    let volume_reports = harness
        .timeline()
        .iter()
        .flat_map(|step| step.reports.iter())
        .filter(|(message_type, _)| message_type == "MESSAGE_TYPE_RNDR_SRVR_VOLUME_CHANGED")
        .count();
    assert_eq!(
        volume_reports,
        1,
        "timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_invariants();
}

/// A spinner must be retractable. The renderer reports BUFFERING while a stream
/// fills; if the load then FAILS, the falling edge still has to go out or the
/// controller spins forever on a load that already gave up.
///
/// This asserts the screen model end of it — that a BUFFERING report draws a
/// spinner and a following settled report clears it. The decision of WHEN to
/// send each is the daemon's report loop, which this tier cannot reach; see the
/// harness module docs.
#[tokio::test]
async fn a_spinner_raised_by_a_load_is_cleared_by_the_next_settled_report() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    harness
        .report_tick_as(ReportTick {
            playing_state: 2,
            buffer_state: BUFFER_STATE_BUFFERING,
            position_ms: 0,
            duration_ms: TRACK_DURATION_MS,
        })
        .await;
    assert_eq!(harness.view().transport, Transport::Buffering);

    harness.advance_playback(2_000);
    harness.report_tick().await;
    assert_eq!(
        harness.view().transport,
        Transport::Playing,
        "the spinner must clear on the next settled report; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_invariants();
}

/// A pause after a GAPLESS advance must not drag playback back to the track the
/// cloud still thinks is playing.
///
/// This is the sharp version of the case above. A gapless hand-off happens
/// inside the player — no command, no report — so for as long as it takes the
/// next report to go out, the cloud's `current_track` names the PREVIOUS track.
/// A state-only pause arriving in that gap carries no track of its own; a
/// renderer that fills the blank from the cloud's stale view aligns the cursor
/// and loads the track that just finished, and the user, who pressed pause,
/// gets the previous song instead.
#[tokio::test]
async fn a_pause_during_a_gapless_advance_does_not_drag_playback_backwards() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    // The player runs out the first track and starts the second on its own. The
    // cloud has not been told; its current_track is still 101.
    harness.advance_gaplessly_to(102);
    harness.advance_playback(3_000);

    let before = harness.engine_calls().len();
    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert_eq!(
        after,
        vec![EngineCall::Pause],
        "a pause must never reload the track the cloud is stale on; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(
        harness.engine().snapshot().track_id,
        102,
        "the audible track must survive the pause"
    );
    harness.assert_invariants();
}

/// The same gap, in the direction that hurts: the previous track ran to its
/// END, so the cloud's cached position is near four minutes. A pause tapped
/// before the driver's track-change report lands must not throw the NEW track
/// to 3:20.
#[tokio::test]
async fn a_pause_during_a_gapless_advance_does_not_jump_to_the_previous_tracks_position() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    // Run the first track almost out and let the cloud hear about it.
    harness.advance_playback(200_000);
    harness.report_tick().await;
    assert_eq!(harness.view().elapsed_ms, Some(200_000));

    // The player rolls into the next track on its own.
    harness.advance_gaplessly_to(102);

    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;

    assert!(
        !harness
            .engine_calls()
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })),
        "a pause must not seek at all here; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(
        harness.engine().snapshot().position,
        0,
        "the new track must still be at its start"
    );
    harness.assert_invariants();
}

/// Every renderer report must quote the queue version the cloud last announced.
///
/// The cloud rejects a report whose `queue_version_ref` is behind its own — the
/// renderer's answer is dropped on the floor, and the controller waits for a
/// state that never comes. So the version has to track the queue, not the
/// session: a push bumps it, and the reports that follow must say so.
#[tokio::test]
async fn reports_quote_the_queue_version_the_cloud_last_announced() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    let announced = harness.queue_snapshot().await.version;
    harness.advance_playback(4_000);
    harness.report_tick().await;
    harness.leave_handoff_echo_window();
    harness.controller().tap_pause().await;

    for step in harness.timeline() {
        for (message_type, payload) in &step.reports {
            let quoted = payload
                .get("queue_version")
                .and_then(|version| {
                    Some((
                        version.get("major")?.as_u64()?,
                        version.get("minor")?.as_u64()?,
                    ))
                })
                .unwrap_or_else(|| {
                    panic!(
                        "'{}' sent {message_type} with no queue_version; timeline:\n{}",
                        step.label,
                        harness.rendered_timeline()
                    )
                });
            assert_eq!(
                quoted,
                (announced.major, announced.minor),
                "'{}' quoted a stale queue version on {message_type}; timeline:\n{}",
                step.label,
                harness.rendered_timeline()
            );
        }
    }

    // And the app's own renderer view agrees with the screen.
    let renderer = harness.renderer_state().await;
    assert_eq!(renderer.playing_state, Some(3));
    assert_eq!(renderer.current_duration_ms, Some(TRACK_DURATION_MS));
    assert_eq!(
        renderer.current_track.map(|item| item.track_id),
        Some(101),
        "the app must still believe it is on the track it started"
    );
    harness.assert_invariants();
}

/// Taking the render back from a peer must not resume OUR old track.
///
/// At `SetActive` time the renderer state still describes what this device last
/// played; the cloud has not yet said where the session actually is, and while
/// the peer held the render it may have moved on. A renderer that loads from
/// that view takes the render back onto the wrong song — observed as 77 seconds
/// into a track the peer was not even playing. `SetActive` flips the flag and
/// nothing else; the authoritative `SetState` follows a few hundred ms later and
/// starts the real track at the real position.
#[tokio::test]
async fn taking_the_render_back_waits_for_the_cloud_before_loading() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(45_000);

    // The phone grabs the render and plays something else entirely.
    harness.peer_takes_the_render().await;

    let before = harness.engine_calls().len();
    harness.controller().take_renderer().await;
    let on_set_active: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert!(
        on_set_active.is_empty(),
        "SetActive must touch the player not at all; it did {on_set_active:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );

    // Now the cloud says where the session really is.
    harness.controller().tap_next_track(777, None).await;
    assert_eq!(
        harness.engine().snapshot().track_id,
        777,
        "the takeback must land on the cloud's track, not ours; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert!(
        harness.engine().snapshot().is_playing,
        "a takeback onto torn-down audio must reload, not bare-resume"
    );
    harness.assert_invariants();
}
