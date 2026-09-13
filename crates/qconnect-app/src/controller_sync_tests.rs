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

use qbz_models::{Quality, RepeatMode};

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
            start_secs: 0,
            quality: Quality::UltraHiRes,
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

    harness.let_the_load_windows_expire();
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
    harness.let_the_load_windows_expire();
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

    // No `let_the_load_windows_expire` — the load just happened.
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
    harness.let_the_load_windows_expire();
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
    harness.let_the_load_windows_expire();
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
    harness.let_the_load_windows_expire();
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

    harness.let_the_load_windows_expire();
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
    harness.let_the_load_windows_expire();
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

// ======================================================== queue and cursor

/// The queue the controller pushed must arrive at the player intact, with the
/// cursor on the track the user picked.
///
/// Materialization is what turns the cloud's list of ids into the player's own
/// queue. Skip it and the audio still starts — the SetState that follows loads
/// the track by itself — so the failure is silent: the right song plays out of
/// an empty queue, and the next track never comes.
#[tokio::test]
async fn a_pushed_queue_arrives_at_the_player_with_the_cursor_on_the_selection() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 1).await;

    let (queue, cursor) = harness.player_queue();
    assert_eq!(
        queue,
        TRACKS.to_vec(),
        "the player must hold the queue the controller pushed; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(
        cursor,
        Some(102),
        "the cursor must name the track the user picked; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_player_queue_matches_the_cloud();
    harness.assert_invariants();
}

/// The same queue pushed twice is materialized once.
///
/// The cloud re-announces the queue on reconnects and on unrelated edits.
/// Rebuilding the player's queue each time tears down the cursor and, with it,
/// whatever was playing.
#[tokio::test]
async fn an_identical_queue_push_is_not_materialized_twice() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    let items = harness.controller().push_queue(&TRACKS, 0).await;
    harness.controller().tap_track(&items, 0).await;
    harness.let_the_load_windows_expire();

    let before = harness.engine_calls().len();
    harness.controller().announce_queue(&items, 0).await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert!(
        !after.iter().any(|call| matches!(
            call,
            EngineCall::SetQueue { .. } | EngineCall::SetQueueWithOrder { .. }
        )),
        "an identical push must not rebuild the queue; it did {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.player_queue().1, Some(101));
    harness.assert_player_queue_matches_the_cloud();
    harness.assert_invariants();
}

/// When the cloud names a track, the player's CURSOR follows it — not just the
/// audio. The cursor is what names the now-playing title in `qbzd status` and
/// in the moOde overlay, so a cursor left behind shows one song's title over
/// another song's audio.
#[tokio::test]
async fn the_cursor_follows_the_track_the_cloud_names() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.let_the_load_windows_expire();

    harness.controller().tap_next_track(103, None).await;

    assert_eq!(
        harness.player_queue().1,
        Some(103),
        "the cursor must land on the track the cloud named; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.engine().snapshot().track_id, 103);
    harness.assert_invariants();
}

// ============================================================ load dedup

/// The cloud re-emitting a SetState for the track already playing must not
/// restart it.
///
/// It re-emits routinely — a next_track correction, a queue_item_id refresh —
/// and re-states `current_position` as 0 each time. Reloading on every one of
/// those is the "first track hiccups on album change" and "needs several taps"
/// report.
#[tokio::test]
async fn the_cloud_re_emitting_the_current_state_does_not_restart_the_track() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(40_000);
    harness.let_the_load_windows_expire();

    let before = harness.engine_calls().len();
    harness
        .controller()
        .reemits_the_current_state(101, 1000)
        .await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert!(
        !after
            .iter()
            .any(|call| matches!(call, EngineCall::StartStream { .. })),
        "a re-emitted state must not reload the track; it did {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(
        harness.engine().snapshot().position,
        40,
        "and it must not drag the clock back to zero either"
    );
    harness.assert_invariants();
}

// ================================================================= volume

/// A volume-up key is a RELATIVE step. The report that goes back must state the
/// ABSOLUTE level it landed on, because that is the number the controller draws
/// on its slider — it does not track the arithmetic itself.
#[tokio::test]
async fn a_relative_volume_step_is_reported_as_the_absolute_level() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    harness.controller().set_volume(40).await;
    harness.controller().nudge_volume(15).await;

    assert!(
        harness
            .engine_calls()
            .contains(&EngineCall::SetVolume { pct: 55 }),
        "the relative step must reach the player as an absolute level; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(
        harness.view().volume,
        Some(55),
        "and the slider must show 55, not the +15; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_invariants();
}

/// Mute silences the player and says so; unmute puts the level back where it
/// was. A renderer that mutes without restoring leaves the controller showing a
/// level nobody can hear.
#[tokio::test]
async fn muting_and_unmuting_round_trips_the_level() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.controller().set_volume(70).await;

    harness.controller().set_muted(true).await;
    assert!(
        harness
            .engine_calls()
            .contains(&EngineCall::SetVolume { pct: 0 }),
        "mute must silence the player; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.view().muted, Some(true));

    harness.controller().set_muted(false).await;
    assert_eq!(
        harness.engine_calls().last(),
        Some(&EngineCall::SetVolume { pct: 70 }),
        "unmute must restore the level the controller still shows; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.view().muted, Some(false));
    harness.assert_invariants();
}

// ======================================= the window where the audio thread lags

/// A SetState naming a track the audio thread has not reached yet must not be
/// seeked.
///
/// This is the ordinary shape of an album change: the push starts track 0, and
/// the cloud's SetState for that same track lands while the stream is still
/// filling. The player therefore still reports the PREVIOUS track, at the
/// previous track's clock, and the frame's `current_position: 0` refers to
/// neither. Acting on it cost 140 ms of dead air and a PCM stop+prepare at the
/// top of every freshly started track — the faint click on each one.
#[tokio::test]
async fn a_setstate_for_a_track_the_audio_thread_has_not_reached_does_not_seek() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(62_000);
    harness.report_tick().await;
    harness.let_the_load_windows_expire();

    // From here a stream opens but stays inaudible until we say so.
    harness.make_the_audio_thread_lag();
    harness.controller().tap_next_track(102, Some(103)).await;

    assert!(
        !harness
            .engine_calls()
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })),
        "the outgoing track's clock must not be seeked on the incoming track's \
         frame; timeline:\n{}",
        harness.rendered_timeline()
    );

    harness.audio_thread_catches_up();
    assert_eq!(harness.engine().snapshot().track_id, 102);
    assert_eq!(
        harness.engine().snapshot().position,
        0,
        "the new track must start at its beginning"
    );
    harness.assert_invariants();
}

/// A takeback loads the peer's track AT the peer's position. Having done so, it
/// must not also seek there.
///
/// The stream already begins at that offset — it waits for the buffer and
/// pre-skips — so a seek behind it is pure waste, and worse than waste: it is
/// either dropped for being past the buffered watermark, or it lands later and
/// rebuilds the engine, re-running the whole multi-second sample skip.
#[tokio::test]
async fn a_takeback_load_at_a_position_is_not_seeked_on_top_of() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.peer_takes_the_render().await;
    harness.let_the_load_windows_expire();

    // The peer is 77 s into a track of its own; the audio thread will need a
    // moment to get there.
    harness.make_the_audio_thread_lag();
    harness.controller().take_renderer().await;
    harness.controller().resume_at(202, 77_000).await;

    let seeks: Vec<_> = harness
        .engine_calls()
        .into_iter()
        .filter(|call| matches!(call, EngineCall::Seek { .. }))
        .collect();
    assert!(
        seeks.is_empty(),
        "the load already started at the offset; it was seeked anyway: {seeks:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );
    assert!(
        harness.engine_calls().contains(&EngineCall::StartStream {
            track_id: 202,
            start_secs: 77,
            quality: Quality::UltraHiRes,
        }),
        "the takeback must stream from the peer's position, not from zero; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.audio_thread_catches_up();
    assert_eq!(harness.engine().snapshot().position, 77);
    harness.assert_invariants();
}

/// A track change that states no position starts the new track at ZERO.
///
/// The cloud does send this shape. The position blank must stay blank — filling
/// it in from the cached renderer state starts the new track wherever the
/// previous one had got to.
#[tokio::test]
async fn a_track_change_without_a_position_starts_at_the_beginning() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(96_000);
    harness.report_tick().await;
    harness.let_the_load_windows_expire();

    harness
        .controller()
        .tap_next_track_without_a_position(102, Some(103))
        .await;

    assert!(
        harness.engine_calls().contains(&EngineCall::StartStream {
            track_id: 102,
            start_secs: 0,
            quality: Quality::UltraHiRes,
        }),
        "a new track with no stated position starts at 0, not at the previous \
         track's 96 s; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.engine().snapshot().position, 0);
    harness.assert_invariants();
}

/// A resume while the cloud's position trails the player by a second must not
/// seek.
///
/// The cloud only hears from the renderer every ~1.8 s, so its cached position
/// is ALWAYS a little behind. A resume whose blank position is filled in from
/// that cache would therefore drag the clock back by a second every single
/// time — an audible stutter on every play tap. The `> 2 s` margin exists for
/// exactly this, and shrinking it is not a harmless tightening.
#[tokio::test]
async fn a_resume_does_not_seek_on_the_ordinary_report_lag() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    // The cloud last heard 30 s...
    harness.advance_playback(30_000);
    harness.report_tick().await;
    // ...and the player has moved on by one report period since.
    harness.advance_playback(1_800);
    harness.let_the_load_windows_expire();

    harness.controller().tap_pause().await;
    harness.controller().tap_play().await;

    assert!(
        !harness
            .engine_calls()
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })),
        "an ordinary report lag is not a seek; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert_eq!(harness.engine().snapshot().position, 31);
    harness.assert_invariants();
}

/// A state-only resume onto torn-down audio must RELOAD, not bare-resume.
///
/// After a hand-off the player still remembers the track id but holds no audio,
/// so a plain resume dies in the audio thread with "cannot resume - no audio
/// data available" — while the cloud goes on reporting paused 0:00 to the
/// controller forever. Deciding on the track id alone is what misses it; the
/// question is whether there is anything loaded.
#[tokio::test]
async fn a_resume_onto_torn_down_audio_reloads_instead_of_dying_quietly() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(20_000);
    harness.report_tick().await;

    // The peer takes the render, which tears our audio down but leaves the
    // track id in place; then it hands the render straight back.
    harness.peer_takes_the_render().await;
    harness.let_the_load_windows_expire();
    harness.controller().take_renderer().await;

    let before = harness.engine_calls().len();
    harness.make_the_audio_thread_lag();
    harness.controller().tap_play().await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert!(
        after
            .iter()
            .any(|call| matches!(call, EngineCall::StartStream { .. })),
        "a resume with no audio loaded must reload; it did {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );
    // And the cold load already began at the cached position, so nothing may
    // seek on top of it: a seek there is either dropped for being past the
    // buffered watermark, or lands later and rebuilds the engine, re-running the
    // whole multi-second sample skip.
    assert!(
        !after
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })),
        "the cold load started at the cached position; it was seeked anyway: {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );

    harness.audio_thread_catches_up();
    assert!(
        harness.engine().snapshot().is_playing,
        "and it must actually be playing afterwards"
    );
    harness.assert_invariants();
}

/// The cloud re-emitting a SetState WHILE THE LOAD IS STILL IN FLIGHT must not
/// start a second stream.
///
/// This is the same re-emission as above, in the window where it actually
/// hurts. The audio thread has not adopted the new track yet, so a check on
/// "what is the player playing" still answers with the OUTGOING track and reads
/// as "not loaded" — which is why the dedup is keyed on the load ATTEMPT
/// instead. Getting this wrong tore the stream down and reopened it, mid-fill,
/// on every routine re-emission.
#[tokio::test]
async fn a_re_emitted_state_during_a_load_does_not_open_a_second_stream() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;
    harness.advance_playback(62_000);
    harness.report_tick().await;
    harness.let_the_load_windows_expire();

    // Track 102 starts loading; nothing is audible yet, so the player still
    // reports 101 at 62 s.
    harness.make_the_audio_thread_lag();
    harness.controller().tap_next_track(102, Some(103)).await;
    let after_first_load = harness.engine_calls().len();

    // The cloud restates the same track mid-load — a next_track correction.
    harness
        .controller()
        .reemits_the_current_state(102, 2000)
        .await;

    let after: Vec<_> = harness
        .engine_calls()
        .into_iter()
        .skip(after_first_load)
        .collect();
    assert!(
        !after
            .iter()
            .any(|call| matches!(call, EngineCall::StartStream { .. })),
        "the re-emission opened a second stream mid-fill: {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );
    assert!(
        !after
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })),
        "and it must not seek the OUTGOING track to the incoming track's 0 \
         either — that is the click at the top of every fresh track: {after:?}\ntimeline:\n{}",
        harness.rendered_timeline()
    );

    harness.audio_thread_catches_up();
    assert_eq!(harness.engine().snapshot().track_id, 102);
    assert_eq!(harness.engine().snapshot().position, 0);
    harness.assert_invariants();
}

// ====================================================== modes the cloud owns

/// Repeat mode from the controller reaches the player, with the right meaning.
///
/// The wire values are 1 = off, 2 = repeat one, 3 = repeat all, and a renderer
/// that maps them by position rather than by value turns "repeat all" into
/// "repeat one" — the queue plays one song forever and the user cannot see why.
#[tokio::test]
async fn the_controllers_repeat_mode_reaches_the_player_with_its_meaning_intact() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    harness.controller().set_loop_mode(3).await;
    assert!(
        harness.engine_calls().contains(&EngineCall::SetRepeatMode {
            mode: RepeatMode::All
        }),
        "loop_mode 3 is repeat-ALL; timeline:\n{}",
        harness.rendered_timeline()
    );

    harness.controller().set_loop_mode(2).await;
    assert_eq!(
        harness.engine_calls().last(),
        Some(&EngineCall::SetRepeatMode {
            mode: RepeatMode::One
        }),
        "loop_mode 2 is repeat-ONE; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_invariants();
}

/// Tapping shuffle flips the FLAG and nothing else.
///
/// QConnect is WS-authoritative for queue order: the cloud decides what the
/// shuffled order is and sends it separately. A renderer that reaches for the
/// order-generating call here invents its own random order, and the phone and
/// the speakers then disagree about what plays next for the rest of the
/// session — the documented "es un infierno" failure.
#[tokio::test]
async fn tapping_shuffle_flips_the_flag_without_inventing_an_order() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    let before = harness.engine_calls().len();
    harness.controller().set_shuffle(true).await;

    let after: Vec<_> = harness.engine_calls().into_iter().skip(before).collect();
    assert_eq!(
        after,
        vec![EngineCall::SetShuffleFlag { enabled: true }],
        "shuffle must set the flag alone — anything that generates a local \
         order diverges from the cloud for good; timeline:\n{}",
        harness.rendered_timeline()
    );
    assert!(
        !after
            .iter()
            .any(|call| matches!(call, EngineCall::SetShuffleWithLocalOrder { .. })),
        "the order-generating call must never be reached from here"
    );
    harness.assert_invariants();
}

/// The quality ceiling the controller announces is the quality the stream is
/// opened at.
///
/// This is the one setting on this path with no audible failure mode. If the
/// ceiling is dropped and the stream defaults to the top, everything works —
/// music plays, the screen is right — and a user who chose CD quality to spare
/// a metered connection silently gets Hi-Res. If it is dropped the other way,
/// a Hi-Res subscriber silently gets MP3. Neither shows up anywhere except the
/// bandwidth bill and the sound.
#[tokio::test]
async fn the_announced_quality_ceiling_is_what_the_stream_opens_at() {
    let harness = ControllerHarness::new().await;
    harness.become_active_renderer().await;

    // The controller announces CD quality before naming a track, as it does on
    // join.
    harness.controller().set_max_audio_quality(2).await;
    harness.controller().push_queue_and_play(&TRACKS, 0).await;

    assert!(
        harness.engine_calls().contains(&EngineCall::StartStream {
            track_id: 101,
            start_secs: 0,
            quality: Quality::Lossless,
        }),
        "a stated ceiling of CD must open a CD stream, not the default top; \
         timeline:\n{}",
        harness.rendered_timeline()
    );

    // Raising the ceiling applies to the next track, not retroactively.
    harness.controller().set_max_audio_quality(4).await;
    harness.let_the_load_windows_expire();
    harness.controller().tap_next_track(102, Some(103)).await;

    assert!(
        harness.engine_calls().contains(&EngineCall::StartStream {
            track_id: 102,
            start_secs: 0,
            quality: Quality::UltraHiRes,
        }),
        "the raised ceiling must reach the next stream; timeline:\n{}",
        harness.rendered_timeline()
    );
    harness.assert_invariants();
}
