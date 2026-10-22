//! Window-free audio and capability qualification. This module never creates
//! Shell, SessionInput, a keyboard hook, or a presentation surface.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use serde_json::json;

use super::{options::CodecChoice, DevOptions};
use crate::{
    audio::{AudioPlayer, OutputMode},
    net,
};

pub(super) fn run(options: &DevOptions) -> Result<()> {
    if options.decoder_probe {
        return decoder_probe(options);
    }
    if options.audio_render_test || options.audio_wasapi {
        require_device_opt_in()?;
    }
    if options.audio_render_test {
        return render_test(options);
    }
    receive(options)
}

fn require_device_opt_in() -> Result<()> {
    if std::env::var("BUBBLEPLAY_AUDIO_DEVICE_TESTS").as_deref() != Ok("1") {
        bail!("real audio tests require BUBBLEPLAY_AUDIO_DEVICE_TESTS=1 on the test VM");
    }
    Ok(())
}

fn decoder_probe(options: &DevOptions) -> Result<()> {
    let _runtime = crate::media::MediaRuntime::new()?;
    // The same hardware-first device as the headless session, with no window creation.
    let device = super::headless::create_device()?;
    let request = match options.codec {
        CodecChoice::Auto => crate::media::CodecRequest::Auto,
        CodecChoice::H264 => crate::media::CodecRequest::Explicit(crate::media::Codec::H264),
        CodecChoice::Hevc => crate::media::CodecRequest::Explicit(crate::media::Codec::Hevc),
    };
    let codec = crate::media::resolve_codec(request, &device)?;
    let candidates = crate::media::availability_report(codec)?;
    super::write_dev_report(
        options,
        &json!({"pass":true,"headless":true,"codec":codec.name(),"candidates":candidates}),
    )
}

struct NetworkRun {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<(), net::NetError>>>,
}

impl NetworkRun {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("audio network worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for NetworkRun {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn receive(options: &DevOptions) -> Result<()> {
    let endpoint = endpoint_for_audio(&options.url)?;
    let mode = if options.audio_wasapi {
        OutputMode::Wasapi
    } else {
        OutputMode::Fake
    };
    let player = AudioPlayer::new(mode)?;
    let control = player.handle();
    control.set_volume(options.audio_volume);
    control.set_muted(options.audio_muted);
    let mut config = net::NetConfig::new(endpoint, net::Codec::H264, net::CookieJar::new());
    config.audio = Some(control.clone());
    config.video_watchdog = false;
    config.transport = options.transport;
    config.ice_checking_timeout = options
        .ice_timeout
        .unwrap_or(net::AUTO_ICE_CHECKING_TIMEOUT);
    let transport = Arc::new(net::TransportStats::new());
    let stop = Arc::new(AtomicBool::new(false));
    let (events_tx, events) = mpsc::sync_channel(8);
    let (_feedback, feedback_rx) = mpsc::channel();
    let mut network = NetworkRun {
        stop: stop.clone(),
        handle: Some(
            thread::Builder::new()
                .name("bubbleplay-audio-wss".into())
                .spawn({
                    let transport = transport.clone();
                    move || net::run(config, events_tx, feedback_rx, stop, transport)
                })?,
        ),
    };
    let started = Instant::now();
    let duration = options.exit_after.unwrap_or(Duration::from_secs(20));
    while started.elapsed() < duration {
        // Drain video events without decoding, presenting or admitting input.
        match events.recv_timeout(Duration::from_millis(10)) {
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if network.handle.as_ref().is_some_and(|h| h.is_finished()) {
            break;
        }
    }
    let result = network.finish();
    drop(player);
    let audio = control.stats();
    let net = transport.snapshot();
    let pass = result.is_ok() && audio.decoded > 0 && audio.decode_errors == 0;
    let document = json!({
        "pass":pass,"headless":true,"sink":if options.audio_wasapi {"wasapi"} else {"fake"},
        "duration_s":started.elapsed().as_secs_f64(),"audio":audio,
        "transport":{"requested":format!("{:?}", options.transport),
            "attempts":net.attempts,"established":net.established,"local_epoch":net.local_epoch,
            "video_received":net.frames_received,"video_acks":net.acks_sent,"keyframe_requests":net.keyframe_requests,
            "webrtc":transport.webrtc_snapshot()},
        "errors":result.as_ref().err().map(|e| e.to_string()),
    });
    super::write_dev_report(options, &document)?;
    result?;
    if !pass {
        bail!("audio session decoded no sound or reported decode errors; inspect the stats file");
    }
    Ok(())
}

fn render_test(options: &DevOptions) -> Result<()> {
    let player = AudioPlayer::new(OutputMode::Wasapi)?;
    let control = player.handle();
    control.set_volume(options.audio_volume);
    control.set_muted(options.audio_muted);
    control.reset_epoch(1);
    let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)?;
    encoder.set_inband_fec(false)?;
    encoder.set_dtx(false)?;
    let started = Instant::now();
    let duration = options.exit_after.unwrap_or(Duration::from_secs(3));
    let mut seq = 0u32;
    let mut next = started;
    while started.elapsed() < duration {
        let mut samples = [0f32; 960];
        for (i, stereo) in samples.chunks_exact_mut(2).enumerate() {
            let t = (u64::from(seq) * 480 + i as u64) as f64 / 48_000.0;
            stereo.fill((t * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.15);
        }
        let mut payload = [0; 1500];
        let len = encoder.encode_float(&samples, &mut payload)?;
        let mut packet = Vec::with_capacity(28 + len);
        packet.extend_from_slice(b"BPA1");
        packet.extend_from_slice(&[1, u8::from(seq == 0), 1, 2]);
        packet.extend_from_slice(&48_000u32.to_be_bytes());
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&(u64::from(seq) * 10_000).to_be_bytes());
        packet.extend_from_slice(&480u16.to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(&payload[..len]);
        control.push(1, &packet, Instant::now());
        seq += 1;
        next += Duration::from_millis(10);
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    drop(player);
    let audio = control.stats();
    let measured = audio.sink_period_ms > 0.0;
    super::write_dev_report(
        options,
        &json!({
            "pass":audio.decoded>0 && audio.decode_errors==0,"headless":true,"sink":"wasapi",
            "device_test":if measured {"measured"} else {"not measured: no usable audio endpoint"},
            "audio":audio,"duration_s":started.elapsed().as_secs_f64(),
        }),
    )
}

fn endpoint_for_audio(value: &str) -> Result<String> {
    let mut url = url::Url::parse(value)?;
    if url.host_str() != Some("127.0.0.1") || !matches!(url.scheme(), "ws" | "wss") {
        bail!("the headless audio harness requires a local fake host on 127.0.0.1");
    }
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid audio host scheme"))?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_harness_refuses_real_hosts_before_starting_workers() {
        assert!(endpoint_for_audio("wss://example.com").is_err());
        assert!(endpoint_for_audio("ws://192.168.1.2:19764").is_err());
        assert!(endpoint_for_audio("ws://127.0.0.1:19764").is_ok());
    }

    /// Build the standalone fake host first. This opt-in test uses no window,
    /// COM endpoint, input capture or real host; it retains JSON evidence.
    #[test]
    #[ignore = "20 s external fake-host test; requires BUBBLEPLAY_AUDIO_E2E_FAKE_HOST and BUBBLEPLAY_AUDIO_E2E_DIR"]
    fn headless_fake_host_tone_loss_and_reorder_for_twenty_seconds() {
        use std::io::BufRead;
        use std::os::windows::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        struct Host {
            child: Child,
            log: Option<JoinHandle<()>>,
        }
        impl Drop for Host {
            fn drop(&mut self) {
                if !matches!(self.child.try_wait(), Ok(Some(_))) {
                    let _ = self.child.kill();
                }
                let _ = self.child.wait();
                if let Some(log) = self.log.take() {
                    let _ = log.join();
                }
            }
        }
        let executable =
            std::env::var_os("BUBBLEPLAY_AUDIO_E2E_FAKE_HOST").expect("fake host executable");
        let directory = std::path::PathBuf::from(
            std::env::var_os("BUBBLEPLAY_AUDIO_E2E_DIR").expect("evidence directory"),
        );
        std::fs::create_dir_all(&directory).unwrap();
        let host_path = directory.join("host.json");
        let client_path = directory.join("client.json");
        let mut host = Host {
            child: Command::new(executable)
                .args([
                    "--audio",
                    "--audio-loss-every",
                    "37",
                    "--audio-reorder-every",
                    "23",
                    "--port",
                ])
                .arg("0")
                .args(["--exit-after", "23", "--stats"])
                .arg(&host_path)
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no console window for the fake host.
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            log: None,
        };
        let stderr = host.child.stderr.take().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        host.log = Some(thread::spawn(move || {
            let mut first = Some(ready_tx);
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if let Some(sender) = first.take() {
                    let _ = sender.send(line);
                }
            }
        }));
        // Ask the owned process which port it bound: never connect through a
        // released port reservation that another local service could acquire.
        let ready = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("fake host ready");
        let address = ready
            .strip_prefix("fake-wss-host: ")
            .and_then(|line| line.split_whitespace().next())
            .expect("fake host listener announcement")
            .trim_end_matches("/ws")
            .to_string();
        let started = Instant::now();
        let options = DevOptions::from_args([
            "--dev-wss".into(),
            address,
            "--dev-audio-only".into(),
            "--exit-after".into(),
            "20".into(),
            "--stats-out".into(),
            client_path.to_string_lossy().into_owned(),
        ])
        .unwrap()
        .unwrap();
        run(&options).unwrap();
        assert!(started.elapsed() >= Duration::from_secs(20));
        loop {
            if let Some(status) = host.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(28),
                "fake host did not stop"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let client: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&client_path).unwrap()).unwrap();
        let host: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&host_path).unwrap()).unwrap();
        assert_eq!(client["headless"], true);
        assert_eq!(client["sink"], "fake");
        assert_eq!(client["pass"], true);
        assert_eq!(host["pass"], true, "{host}");
        let audio = &client["audio"];
        let pcm_hz = audio["output_frequency_hz"].as_f64().unwrap();
        let pcm_rms = audio["output_rms"].as_f64().unwrap();
        assert!((430.0..450.0).contains(&pcm_hz), "tone frequency {pcm_hz}");
        assert!((0.14..0.20).contains(&pcm_rms), "tone energy {pcm_rms}");
        assert!(audio["decoded"].as_u64().unwrap() >= 1800, "{audio}");
        assert_eq!(audio["decode_errors"], 0);
        assert_eq!(audio["device_errors"], 0);
        assert!(audio["depth_max_ms"].as_f64().unwrap() <= 150.0, "{audio}");
        assert!(
            audio["receipt_to_render_max_ms"].as_f64().unwrap() <= 150.0,
            "{audio}"
        );
        let lost = host["audio"]["lost_by_script"].as_u64().unwrap();
        let plc = audio["concealed"].as_u64().unwrap();
        assert!(
            lost >= 40 && host["audio"]["reordered"].as_u64().unwrap() >= 50,
            "{host}"
        );
        // At most one lost frame straddles teardown; silence adds at most
        // three PLC frames. Allow one further frame at a timer boundary.
        assert!(
            (lost.saturating_sub(1)..=lost + 4).contains(&plc),
            "PLC {plc}, scripted loss {lost}: {audio}"
        );
        println!(
            "20 s fake audio: {pcm_hz:.2} Hz, RMS {pcm_rms:.4}, lost {lost}, PLC {plc}; {}",
            directory.display()
        );
    }
}
