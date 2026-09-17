//! Shared cleanup for raw CPAL output-device enumeration.
//!
//! CPAL's Linux/ALSA host exposes *every* libasound PCM: the `null` sink
//! ("Discard all samples …"), the bare routing/format plugins, and one
//! entry per card × profile. Worse, several distinct PCM ids collapse to
//! the **same** human description — so a raw `host.output_devices()` dump
//! shows "Discard all samples", 8× "HDA Intel PCH, ALC3254 Analog", N×
//! "Cambridge Audio USB Audio 2.0", etc. That is useless as a picker.
//!
//! The precise pickers (the ALSA + PipeWire backends) build their lists
//! from `/proc/asound` and `pactl`, so they already produce one entry per
//! real output. The CPAL-default path (`CpalDefaultBackend` = "System",
//! and the JACK placeholder) and the `output_sinks` diagnostic had **no**
//! filtering at all — this module closes that gap with the same intent
//! the ALSA backend already encodes: *deduplicated entries, real outputs
//! first.* The `null` discard sink is kept (it is a legitimate "send to
//! nowhere" target) but always sorted to the **end** of the list, never
//! offered as a first-class output.
//!
//! Host-agnostic on purpose: PipeWire node names (`alsa_output.*`) and
//! macOS/Windows device names carry unique displays, so they pass through
//! untouched (the helper is a no-op there beyond dropping a stray discard
//! sink). Pure string logic — unit-tested without any audio host.

/// True for the ALSA `null` PCM, whose CPAL description is
/// "Discard all samples (playback) or generate zero samples (capture)".
/// It never reaches hardware, so it must never appear in an output picker.
pub fn is_discard_sink(display: &str) -> bool {
    display
        .trim()
        .to_ascii_lowercase()
        .starts_with("discard all samples")
}

/// Dedup grain: distinct real outputs carry distinct human descriptions
/// (the ALSA host names analog "… Analog", S/PDIF "… Digital", "HDMI 0/1",
/// each USB DAC by its product string), while the plugin-wrapper flavors
/// of one output (`front:`/`hw:`/`plughw:`/`surround*:`/`plug:` over the
/// same card+device) all share one description. Folding on the normalized
/// description collapses the wrappers and keeps the genuinely distinct
/// outputs.
fn dedup_key(display: &str) -> String {
    display
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Preference within a display group (lower = kept). When several PCM ids
/// share a description we keep the cleanest, most-openable id: the system
/// default and PipeWire/Pulse server sinks first, then PipeWire nodes, then
/// the `front:`/`sysdefault:` card aliases, leaving the `surround*`/`plug:`/
/// raw-`hw:` wrappers as last resort.
fn id_rank(id: &str) -> u8 {
    match id {
        "default" | "pipewire" | "pulse" | "sysdefault" => return 0,
        _ => {}
    }
    if id.starts_with("alsa_output.") {
        1
    } else if id.starts_with("front:CARD=") {
        2
    } else if id.starts_with("sysdefault:CARD=") {
        3
    } else if id.starts_with("iec958:CARD=") || id.starts_with("hdmi:CARD=") {
        4
    } else if id.starts_with("hw:") || id.starts_with("plughw:") {
        6
    } else if id.starts_with("surround")
        || id.starts_with("plug:")
        || id.starts_with("dmix")
        || id.starts_with("dsnoop")
        || id.starts_with("route")
    {
        9
    } else {
        5
    }
}

/// Collapse entries that share a display name (keeping the best-ranked id of
/// each group), drop blank rows, and emit **real outputs first** in first-seen
/// order with the `null` discard sink(s) pushed to the end.
///
/// Generic over the caller's row type so both the `AudioDevice` enumeration
/// and the `OutputSinkInfo` diagnostic reuse one tested implementation:
/// `id_of` yields the re-openable device id, `display_of` the shown name.
pub fn retain_real_outputs<T>(
    items: Vec<T>,
    id_of: impl Fn(&T) -> &str,
    display_of: impl Fn(&T) -> &str,
) -> Vec<T> {
    use std::collections::HashMap;

    // First pass: pick the winning index per display group, in first-seen order.
    // Discard sinks are tracked separately so they can be appended last.
    let mut winner: HashMap<String, usize> = HashMap::new();
    let mut real_order: Vec<String> = Vec::new();
    let mut discard_order: Vec<String> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let display = display_of(item);
        if display.trim().is_empty() {
            continue;
        }
        let key = dedup_key(display);
        match winner.get(&key).copied() {
            None => {
                winner.insert(key.clone(), i);
                if is_discard_sink(display) {
                    discard_order.push(key);
                } else {
                    real_order.push(key);
                }
            }
            Some(cur) => {
                if id_rank(id_of(item)) < id_rank(id_of(&items[cur])) {
                    winner.insert(key, i);
                }
            }
        }
    }

    // Second pass: emit real outputs first (first-seen order), then discard.
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(real_order.len() + discard_order.len());
    for key in real_order.into_iter().chain(discard_order) {
        if let Some(item) = slots[winner[&key]].take() {
            out.push(item);
        }
    }
    out
}

/// True when a PCM id may be handed to libasound's hardware-parameter probe
/// (`snd_pcm_open` + `snd_pcm_hw_params_get_*`) without risking the process.
///
/// **This is a crash guard, not a preference.** libasound answers a probe of a
/// plugin PCM whose parameter space refines to empty with
/// `snd1_pcm_hw_param_get_min: Assertion '!snd_interval_empty(i)' failed` —
/// an `assert()`, so `abort()`, inside C. Rust cannot catch it: the daemon
/// simply disappears, mid-poll, with nothing in the log. moOde hit exactly
/// this: turning PeppyALSA on inserts a `softvol` -> `meter` -> `_peppyout`
/// chain into the ALSA namespace, and the next enumeration killed pibuz
/// within seconds, over and over, for as long as the watchdog restarted it.
///
/// So only the shapes that resolve straight to a kernel PCM are probe-safe.
/// Everything else — the moOde chain (`_audioout`, `peppy`, `peppyalsa`,
/// `softvol_and_peppyalsa`, `camilladsp`, `crossfeed`, `alsaequal`,
/// `eqfa12p`, `invpolarity`, `trx_send`), and any other third-party plugin a
/// distro drops into `/etc/alsa/conf.d` — is listed and opened normally but
/// never *probed*. Note that `default` is deliberately excluded: `pcm.!default`
/// is a distro's to redefine, so it can be a plugin chain wearing an
/// alsa-lib name. (moOde leaves it alone — this is caution, not a known case.)
///
/// Non-Linux PCM ids never match, which is correct — this predicate only
/// guards the ALSA probe, and callers on other platforms skip the check.
pub fn is_probe_safe_pcm_id(id: &str) -> bool {
    let id = id.trim();
    // Kernel PCMs, numeric (`hw:2,0`) or by-name (`hw:CARD=IQaudIODAC,DEV=0`).
    if let Some(rest) = id
        .strip_prefix("hw:")
        .or_else(|| id.strip_prefix("plughw:"))
    {
        return !rest.is_empty();
    }
    // alsa-lib's own per-card aliases. These are generated by alsa-lib from the
    // card itself, not by a conf.d drop-in, so their slave is always a kernel
    // PCM.
    for prefix in [
        "front:CARD=",
        "sysdefault:CARD=",
        "iec958:CARD=",
        "hdmi:CARD=",
    ] {
        if id.starts_with(prefix) {
            return true;
        }
    }
    // Sound-server PCMs: no kernel plugin chain behind them, and the PipeWire
    // backend has to probe the one it selects.
    matches!(id, "pipewire" | "pulse")
}

/// The ALSA PCM ids that name something other than a definition in the config
/// tree: alsa-lib's own aliases and the sound-server bridges.
///
/// The one list, so [`is_named_config_pcm`] and
/// `AlsaDirectStream::supports_direct_open` cannot drift apart.
pub const GENERIC_ALSA_ALIASES: &[&str] = &[
    "default",
    "sysdefault",
    "pulse",
    "pipewire",
    "jack",
    "null",
    "oss",
    "speex",
    "upmix",
    "vdownmix",
];

/// True for a PCM id that names a definition in the ALSA config tree —
/// moOde's `_audioout` and `peppy`, a CamillaDSP or equalizer chain, anything
/// a distro drops into `/etc/alsa/conf.d`.
///
/// Deliberately NOT the complement of [`is_probe_safe_pcm_id`]. The generic
/// aliases are not probe-safe either, but rodio is the only way to open them,
/// so treating them the same way would leave no path at all.
pub fn is_named_config_pcm(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && !id.contains(':') && !GENERIC_ALSA_ALIASES.contains(&id)
}

/// True when a device id must not be handed to rodio.
///
/// `DeviceSinkBuilder::from_device` asks cpal for the device's default output
/// config, and cpal answers by opening the PCM and reading its
/// hardware-parameter space — the same call that `abort()`s on a plugin chain
/// (see [`is_probe_safe_pcm_id`]). A named config PCM belongs to the direct
/// ALSA path, which opens it without asking it anything, so there is nothing
/// lost by refusing it here and a daemon to lose by not.
///
/// Off Linux there is no libasound and no ALSA config tree, so nothing is
/// refused — a CoreAudio device name like "MacBook Pro Speakers" looks exactly
/// like a named PCM and must keep working.
pub fn must_not_reach_rodio(device_id: &str) -> bool {
    cfg!(target_os = "linux") && is_named_config_pcm(device_id)
}

/// The raw backend id behind a CPAL device — on ALSA the PCM id (`peppy`,
/// `hw:CARD=IQaudIODAC,DEV=0`).
///
/// NOT `description().name()`: that is the human label ("IQaudIODAC, IQaudIO
/// DAC HiFi pcm512x-hifi-0"), a different string, and treating the two as
/// interchangeable is how a lookup keyed on PCM ids came to match nothing.
pub fn cpal_pcm_id(device: &rodio::cpal::Device) -> Option<String> {
    use rodio::cpal::traits::DeviceTrait;
    device.id().ok().map(|id| id.1)
}

/// [`is_probe_safe_pcm_id`] for a live CPAL device.
///
/// Off Linux there is no libasound to abort, so every device is probe-safe; a
/// device whose id cannot be read is not, because we cannot tell what it is.
pub fn is_probe_safe_device(device: &rodio::cpal::Device) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    cpal_pcm_id(device).is_some_and(|id| is_probe_safe_pcm_id(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direct path and the rodio guard must classify a PCM the same way.
    /// If `supports_direct_open` declined something `is_named_config_pcm` also
    /// declines, that PCM falls through to rodio — which is the abort.
    #[test]
    fn named_config_pcms_are_the_ones_the_direct_path_owns() {
        for id in ["_audioout", "_peppyout", "peppy", "peppyalsa", "camilladsp"] {
            assert!(is_named_config_pcm(id), "{id} is a conf.d PCM");
        }
        // Aliases and server bridges: rodio is the only way to open these, so
        // they must NOT be classified as named config PCMs.
        for id in GENERIC_ALSA_ALIASES {
            assert!(!is_named_config_pcm(id), "{id} is an alias, not a chain");
        }
        // Anything with a colon is a card-backed id, not a config name.
        for id in ["hw:2,0", "plughw:CARD=X,DEV=0", "front:CARD=PCH,DEV=0"] {
            assert!(!is_named_config_pcm(id));
        }
        assert!(!is_named_config_pcm(""));
        assert!(!is_named_config_pcm("   "));
    }

    /// The rodio guard is Linux-only on purpose: a CoreAudio device name has
    /// no colon and is not an alias, so the bare predicate would refuse every
    /// named output on macOS.
    #[test]
    fn the_rodio_guard_only_bites_on_linux() {
        assert!(is_named_config_pcm("MacBook Pro Speakers"));
        assert_eq!(
            must_not_reach_rodio("MacBook Pro Speakers"),
            cfg!(target_os = "linux")
        );
        assert!(!must_not_reach_rodio("default"));
        assert!(!must_not_reach_rodio("hw:2,0"));
    }

    /// The exact PCM namespace of the moOde box that crash-looped (forum
    /// #4660, pibuz 2.4.1): every plugin PCM its `/etc/alsa/conf.d` defines
    /// must be refused, or the probe abort()s the daemon again.
    #[test]
    fn moode_plugin_pcms_are_never_probed() {
        for id in [
            "_audioout",
            "_peppyout",
            "peppy",
            "peppyalsa",
            "softvol_and_peppyalsa",
            "trx_send",
            "camilladsp",
            "crossfeed",
            "alsaequal",
            "eqfa12p",
            "invpolarity",
            "btstream",
        ] {
            assert!(
                !is_probe_safe_pcm_id(id),
                "{id} is a conf.d plugin PCM and must not be probed"
            );
        }
    }

    #[test]
    fn kernel_pcms_and_card_aliases_are_probe_safe() {
        for id in [
            "hw:2,0",
            "hw:CARD=IQaudIODAC,DEV=0",
            "plughw:2,0",
            "plughw:CARD=IQaudIODAC,DEV=0",
            "front:CARD=PCH,DEV=0",
            "sysdefault:CARD=PCH",
            "iec958:CARD=PCH,DEV=0",
            "hdmi:CARD=vc4hdmi0,DEV=0",
            "pipewire",
            "pulse",
        ] {
            assert!(is_probe_safe_pcm_id(id), "{id} should be probe-safe");
        }
    }

    /// `default` is a distro-editable alias — moOde points it at its own
    /// plugin chain — so it is NOT probe-safe even though it looks harmless.
    /// Same for a bare `hw:` with nothing after the colon.
    #[test]
    fn redefinable_and_malformed_ids_are_refused() {
        assert!(!is_probe_safe_pcm_id("default"));
        assert!(!is_probe_safe_pcm_id("sysdefault"));
        assert!(!is_probe_safe_pcm_id("null"));
        assert!(!is_probe_safe_pcm_id("hw:"));
        assert!(!is_probe_safe_pcm_id("plughw:"));
        assert!(!is_probe_safe_pcm_id(""));
    }

    fn run(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        let owned: Vec<(String, String)> = rows
            .iter()
            .map(|(id, d)| (id.to_string(), d.to_string()))
            .collect();
        retain_real_outputs(owned, |r| r.0.as_str(), |r| r.1.as_str())
    }

    #[test]
    fn discard_sink_sorted_to_end() {
        assert!(is_discard_sink(
            "Discard all samples (playback) or generate zero samples (capture)"
        ));
        // null appears FIRST in the raw list but must be emitted last.
        let out = run(&[
            (
                "null",
                "Discard all samples (playback) or generate zero samples (capture)",
            ),
            (
                "default",
                "Default ALSA Output (currently PipeWire Media Server)",
            ),
            ("front:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
        ]);
        let ids: Vec<&str> = out.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, vec!["default", "front:CARD=PCH,DEV=0", "null"]);
    }

    #[test]
    fn collapses_plugin_wrappers_to_one_per_output() {
        // The exact shape of the user's listota: one analog output exposed
        // via many plugin ids that all share a description.
        let out = run(&[
            ("front:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
            ("surround51:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
            ("hw:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
            ("plughw:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
        ]);
        assert_eq!(out.len(), 1);
        // front: outranks surround/hw/plughw.
        assert_eq!(out[0].0, "front:CARD=PCH,DEV=0");
    }

    #[test]
    fn keeps_genuinely_distinct_outputs() {
        let out = run(&[
            (
                "default",
                "Default ALSA Output (currently PipeWire Media Server)",
            ),
            ("front:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3254 Analog"),
            ("iec958:CARD=PCH,DEV=1", "HDA Intel PCH, ALC3254 Digital"),
            ("hdmi:CARD=PCH,DEV=3", "HDA Intel PCH, HDMI 0"),
            (
                "front:CARD=C20,DEV=0",
                "Cambridge Audio USB Audio 2.0, USB Audio",
            ),
            (
                "surround40:CARD=C20,DEV=0",
                "Cambridge Audio USB Audio 2.0, USB Audio",
            ),
        ]);
        let ids: Vec<&str> = out.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "default",
                "front:CARD=PCH,DEV=0",
                "iec958:CARD=PCH,DEV=1",
                "hdmi:CARD=PCH,DEV=3",
                "front:CARD=C20,DEV=0",
            ]
        );
    }

    #[test]
    fn passes_pipewire_node_names_through() {
        let out = run(&[
            ("alsa_output.usb-Cambridge", "alsa_output.usb-Cambridge"),
            (
                "alsa_output.pci-0000_00_1f.3",
                "alsa_output.pci-0000_00_1f.3",
            ),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn first_seen_order_is_preserved() {
        let out = run(&[
            (
                "hw:CARD=C20,DEV=0",
                "Cambridge Audio USB Audio 2.0, USB Audio",
            ),
            ("default", "Default ALSA Output"),
            // Better-ranked id for Cambridge appears later; it wins the group
            // but the group keeps its first-seen position (before Default).
            (
                "front:CARD=C20,DEV=0",
                "Cambridge Audio USB Audio 2.0, USB Audio",
            ),
        ]);
        let ids: Vec<&str> = out.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, vec!["front:CARD=C20,DEV=0", "default"]);
    }

    #[test]
    fn drops_blank_displays() {
        let out = run(&[("weird", "   "), ("default", "Default ALSA Output")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "default");
    }
}
