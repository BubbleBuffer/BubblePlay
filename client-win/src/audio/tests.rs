use super::wire::{Packet, FRAME_SAMPLES, SAMPLE_RATE};
use super::*;

fn unhex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|part| u8::from_str_radix(std::str::from_utf8(part).unwrap(), 16).unwrap())
        .collect()
}

fn wire(packet: &Packet) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"BPA1");
    bytes.extend_from_slice(&[1, packet.discontinuity as u8, 1, 2]);
    bytes.extend_from_slice(&SAMPLE_RATE.to_be_bytes());
    bytes.extend_from_slice(&packet.seq.to_be_bytes());
    bytes.extend_from_slice(&packet.capture_ts_us.to_be_bytes());
    bytes.extend_from_slice(&(FRAME_SAMPLES as u16).to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&packet.payload);
    bytes
}

fn tones(count: usize, first: u32) -> Vec<Packet> {
    let mut encoder = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::Audio,
    )
    .unwrap();
    encoder.set_inband_fec(false).unwrap();
    let mut payload = vec![0u8; 1500];
    (0..count)
        .map(|frame| {
            let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2);
            for sample in 0..FRAME_SAMPLES {
                let phase = (frame * FRAME_SAMPLES + sample) as f32 * std::f32::consts::TAU * 440.0
                    / SAMPLE_RATE as f32;
                let value = phase.sin() * 0.3;
                pcm.extend_from_slice(&[value, value]);
            }
            let bytes = encoder.encode_float(&pcm, &mut payload).unwrap();
            Packet {
                seq: first.wrapping_add(frame as u32),
                capture_ts_us: frame as u64 * 10_000,
                discontinuity: frame == 0,
                payload: payload[..bytes].to_vec(),
            }
        })
        .collect()
}

fn push(engine: &mut Engine, packet: Packet, now_us: u64) {
    engine.push(
        TimedPacket {
            packet,
            received_us: now_us,
        },
        now_us,
    );
}

#[derive(Default)]
struct Recorder {
    fake: FakeSink,
    output: Vec<f32>,
    clears: usize,
    fail_write: bool,
    padding_ms: f64,
    hardware: bool,
}
impl AudioSink for Recorder {
    fn writable(&mut self, now: u64) -> Result<(usize, f64)> {
        let (frames, _) = self.fake.writable(now)?;
        Ok((frames, self.padding_ms))
    }
    fn write(&mut self, pcm: &[f32]) -> Result<()> {
        if self.fail_write {
            anyhow::bail!("device invalidated");
        }
        self.output.extend_from_slice(pcm);
        self.fake.write(pcm)
    }
    fn clear(&mut self) -> Result<()> {
        self.clears += 1;
        self.padding_ms = 0.0;
        self.fake.clear()
    }
    fn wait(&self, _: u32) {}
    fn buffer_ms(&self) -> f64 {
        self.fake.buffer_ms()
    }
    fn period_ms(&self) -> f64 {
        self.fake.period_ms()
    }
    fn observes_device_padding(&self) -> bool {
        self.hardware
    }
}
fn render(engine: &mut Engine, sink: &mut Recorder, from: u64, until: u64) {
    for now in (from..until).step_by(5000) {
        engine.render(sink, now, 1.0, false).unwrap();
    }
}

#[test]
fn every_shared_fixture_acceptance_and_fields_match() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/audio-contract.json")).unwrap();
    for case in fixture["packets"].as_array().unwrap() {
        let bytes = if let Some(hex) = case["hex"].as_str() {
            unhex(hex)
        } else {
            let mut bytes = unhex(case["hexPrefix"].as_str().unwrap());
            bytes.resize(
                bytes.len() + case["payloadBytes"].as_u64().unwrap() as usize,
                0,
            );
            bytes
        };
        let actual = parse_packet(&bytes);
        assert_eq!(
            actual.is_some(),
            case["accepted"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
        if let Some(packet) = actual {
            assert_eq!(packet.seq as u64, case["seq"].as_u64().unwrap());
            assert_eq!(
                packet.capture_ts_us.to_string(),
                case["captureTsUs"].as_str().unwrap()
            );
            assert_eq!(
                packet.discontinuity,
                case["discontinuity"].as_bool().unwrap()
            );
            assert_eq!(packet.payload, unhex(case["payloadHex"].as_str().unwrap()));
        }
    }
}

#[test]
fn recognition_is_magic_only_and_parser_never_panics_on_truncation() {
    assert!(is_audio_packet(b"BPA1"));
    assert!(!is_audio_packet(b"BPA"));
    let valid = wire(&tones(1, 0).remove(0));
    for len in 0..=28 {
        assert!(parse_packet(&valid[..len]).is_none());
    }
    let mut max = valid[..28].to_vec();
    max.resize(1528, 0);
    assert!(parse_packet(&max).is_some());
    max.push(0);
    assert!(parse_packet(&max).is_none());
}

#[test]
fn forty_ms_start_lead_and_real_opus_tone_energy_frequency() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    let packets = tones(100, 0);
    for now in (0..1_000_000u64).step_by(5000) {
        if now % 10_000 == 0 {
            push(&mut engine, packets[(now / 10_000) as usize].clone(), now);
        }
        engine.render(&mut sink, now, 1.0, false).unwrap();
    }
    assert!(sink.output[..1920 * 2].iter().all(|s| *s == 0.0));
    let stats = engine.stats();
    assert_eq!(stats.concealed, 0);
    assert!(stats.decoded >= 95);
    assert!((0.18..0.24).contains(&stats.output_rms), "{stats:?}");
    assert!(
        (430.0..450.0).contains(&stats.output_frequency_hz),
        "{stats:?}"
    );
    assert!((39.0..41.0).contains(&stats.receipt_to_render_p50_ms.unwrap()));
    assert!(stats.depth_p95_ms.unwrap() <= 60.0);
}

#[test]
fn reorder_before_deadline_is_decoded_in_sequence_without_plc() {
    let packets = tones(5, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for index in [0, 2, 1, 4, 3] {
        push(&mut engine, packets[index].clone(), index as u64 * 1000);
    }
    render(&mut engine, &mut sink, 0, 90_000);
    let stats = engine.stats();
    assert_eq!(stats.decoded, 5);
    assert_eq!(stats.concealed, 0);
    assert_eq!(stats.dropped, 0);
}

#[test]
fn initial_unflagged_reorder_can_precede_first_arrival() {
    let mut packets = tones(3, 9);
    packets[0].discontinuity = false;
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, packets[1].clone(), 0);
    push(&mut engine, packets[0].clone(), 1000);
    push(&mut engine, packets[2].clone(), 2000);
    render(&mut engine, &mut sink, 0, 70_000);
    assert_eq!(engine.stats().decoded, 3);
    assert_eq!(engine.stats().dropped, 0);
}

#[test]
fn one_missing_packet_uses_opus_plc_and_next_packet_survives() {
    let packets = tones(5, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for index in [0, 1, 3, 4] {
        push(&mut engine, packets[index].clone(), 0);
    }
    render(&mut engine, &mut sink, 0, 90_000);
    let stats = engine.stats();
    assert_eq!(stats.decoded, 4);
    assert_eq!(stats.concealed, 1);
    assert_eq!(stats.underruns, 1);
    assert!(stats.output_rms > 0.1);
}

#[test]
fn duplicate_discontinuity_does_not_reset_and_late_packet_cannot_replay() {
    let packets = tones(2, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, packets[0].clone(), 0);
    push(&mut engine, packets[0].clone(), 1000);
    push(&mut engine, packets[1].clone(), 2000);
    render(&mut engine, &mut sink, 0, 60_000);
    push(&mut engine, packets[0].clone(), 60_000);
    let stats = engine.stats();
    assert_eq!(stats.duplicates, 1);
    assert_eq!(stats.late, 1);
    assert_eq!(stats.decoded, 2);
    assert_eq!(stats.schedule_resets, 0);
}

#[test]
fn sequence_wrap_is_continuous() {
    let packets = tones(4, u32::MAX - 1);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for packet in packets {
        push(&mut engine, packet, 0);
    }
    render(&mut engine, &mut sink, 0, 80_000);
    assert_eq!(engine.stats().decoded, 4);
    assert_eq!(engine.stats().dropped, 0);
    assert_eq!(engine.stats().concealed, 0);
}

#[test]
fn discontinuity_flushes_decoded_pcm_and_restarts_lead() {
    let mut packets = tones(3, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, packets[0].clone(), 0);
    render(&mut engine, &mut sink, 0, 45_000); // half a decoded packet remains
    packets[2].discontinuity = true;
    push(&mut engine, packets[2].clone(), 45_000);
    let start = sink.output.len();
    render(&mut engine, &mut sink, 45_000, 85_000);
    assert!(sink.output[start..].iter().all(|s| *s == 0.0));
    assert_eq!(sink.clears, 1);
    render(&mut engine, &mut sink, 85_000, 95_000);
    assert_eq!(engine.stats().decoded, 2);
}

#[test]
fn burst_overgrowth_discards_oldest_and_keeps_depth_below_150ms() {
    let packets = tones(100, 0);
    let mut engine = Engine::new().unwrap();
    for packet in packets {
        push(&mut engine, packet, 0);
        assert!(engine.stats().depth_ms <= 150.0);
    }
    assert!(engine.stats().dropped > 80);
}

#[test]
fn sparse_big_gap_cannot_leave_large_logical_depth() {
    let mut packets = tones(2, 0);
    packets[1].seq = 90;
    let mut engine = Engine::new().unwrap();
    push(&mut engine, packets[0].clone(), 0);
    push(&mut engine, packets[1].clone(), 0);
    assert!(engine.stats().depth_ms <= 60.0, "{:?}", engine.stats());
    assert!(engine.stats().dropped > 0);
}

#[test]
fn long_gap_does_not_generate_unbounded_plc() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, tones(1, 0).remove(0), 0);
    render(&mut engine, &mut sink, 0, 2_000_000);
    assert_eq!(engine.stats().concealed, 3);
    assert_eq!(engine.stats().underruns, 4);
    assert_eq!(engine.stats().depth_ms, 0.0);
}

#[test]
fn sustained_excess_and_clock_drift_remain_bounded() {
    let packets = tones(600, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for (i, packet) in packets.into_iter().enumerate() {
        // Network delivers 10 ms of audio every 5 ms, beyond any clock drift.
        let now = i as u64 * 5000;
        push(&mut engine, packet, now);
        engine.render(&mut sink, now, 1.0, false).unwrap();
        assert!(engine.stats().depth_ms <= 150.0);
    }
    assert!(engine.stats().decoded > 200);
    assert!(engine.stats().dropped > 100);
    assert!(engine.stats().receipt_to_render_p95_ms.unwrap() <= 150.0);
    assert!(engine.stats().receipt_to_render_max_ms.unwrap() <= 150.0);
    assert!(engine.stats().depth_max_ms <= 150.0);
}

#[test]
fn slower_faster_sender_and_slower_faster_device_clocks_remain_bounded() {
    struct RateSink {
        frames: usize,
    }
    impl AudioSink for RateSink {
        fn writable(&mut self, _: u64) -> Result<(usize, f64)> {
            Ok((self.frames, 0.0))
        }
        fn write(&mut self, _: &[f32]) -> Result<()> {
            Ok(())
        }
        fn clear(&mut self) -> Result<()> {
            Ok(())
        }
        fn wait(&self, _: u32) {}
        fn buffer_ms(&self) -> f64 {
            5.0
        }
        fn period_ms(&self) -> f64 {
            5.0
        }
    }
    let packets = tones(2200, 0);
    // Sender runs +/-5%; device runs +/-0.83%. These exceed usual clock
    // drift so a 20 s run must exercise correction in both directions.
    for (sender_interval, output_frames) in
        [(9500u64, 240), (10_500, 240), (10_000, 238), (10_000, 242)]
    {
        let mut engine = Engine::new().unwrap();
        let mut sink = RateSink {
            frames: output_frames,
        };
        let mut next_packet = 0u64;
        let mut seq = 0;
        for now in (0..20_000_000u64).step_by(5000) {
            while next_packet <= now {
                engine.push(
                    TimedPacket {
                        packet: packets[seq].clone(),
                        received_us: next_packet,
                    },
                    now,
                );
                seq += 1;
                next_packet += sender_interval;
            }
            engine.render(&mut sink, now, 1.0, false).unwrap();
        }
        let stats = engine.stats();
        assert!(
            stats.decoded > 1000,
            "interval={sender_interval}, frames={output_frames}, {stats:?}"
        );
        assert!(stats.depth_max_ms <= 150.0, "{stats:?}");
        assert!(
            stats.receipt_to_render_max_ms.unwrap() <= 150.0,
            "{stats:?}"
        );
        assert!(
            stats.dropped > 0 || stats.concealed > 0,
            "drift must cause bounded correction: {stats:?}"
        );
    }
}

#[test]
fn bound_includes_device_padding_and_counts_empty_device_observations() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder {
        padding_ms: 100.0,
        hardware: true,
        ..Default::default()
    };
    for packet in tones(10, 0) {
        push(&mut engine, packet, 0);
    }
    engine.render(&mut sink, 0, 1.0, false).unwrap();
    assert!(
        sink.clears > 0,
        "software+device queue over 150 ms must flush"
    );
    assert!(engine.stats().depth_max_ms <= 150.0);
    engine.render(&mut sink, 5000, 1.0, false).unwrap();
    assert!(engine.stats().device_empty_observations > 0);
}

#[test]
fn renderer_stall_discards_aged_audio_instead_of_replaying_it() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for packet in tones(5, 0) {
        push(&mut engine, packet, 0);
    }
    render(&mut engine, &mut sink, 0, 45_000);
    let before = sink.output.len();
    engine.render(&mut sink, 500_000, 1.0, false).unwrap();
    assert!(engine.stats().dropped > 0);
    assert!(sink.output[before..].iter().all(|s| s.abs() < 0.000001));
    assert!(engine.stats().receipt_to_render_max_ms.unwrap() <= 150.0);
}

#[test]
fn leftover_pcm_uses_projected_device_time_and_max_covers_final_sample() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, tones(1, 0).remove(0), 0);
    // Decode at age140ms and retain the packet's last5ms in software.
    engine.render(&mut sink, 140_000, 1.0, false).unwrap();
    assert!((144.9..145.0).contains(&engine.stats().receipt_to_render_max_ms.unwrap()));
    let before = sink.output.len();
    sink.padding_ms = 10.0;
    // Callback age145ms appears acceptable; actual output is155–160ms.
    engine.render(&mut sink, 145_000, 1.0, false).unwrap();
    assert!(sink.output[before..].iter().all(|s| s.abs() < 0.000001));
    assert!(engine.stats().dropped >= 1);
    assert!(engine.stats().receipt_to_render_max_ms.unwrap() <= 150.0);
}

#[test]
fn packet_crossing_max_age_within_one_write_drops_its_tail() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder {
        padding_ms: 8.0,
        ..Default::default()
    };
    push(&mut engine, tones(1, 0).remove(0), 0);
    engine.render(&mut sink, 140_000, 1.0, false).unwrap();
    // Last admissible sample is index96 at150ms. All later samples must
    // have abandoned the original packet even within this single write.
    assert!(sink.output[97 * 2..].iter().all(|s| s.abs() < 0.000001));
    assert!(engine.stats().dropped >= 1);
    assert_eq!(engine.stats().receipt_to_render_max_ms, Some(150.0));
}

#[test]
fn twenty_ms_opus_payload_is_refused_by_fixed_duration_decoder() {
    let mut encoder = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::Audio,
    )
    .unwrap();
    let mut data = vec![0u8; 1500];
    let len = encoder
        .encode_float(&vec![0.1f32; 1920], &mut data)
        .unwrap();
    let packet = Packet {
        seq: 0,
        capture_ts_us: 0,
        discontinuity: true,
        payload: data[..len].to_vec(),
    };
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, packet, 0);
    render(&mut engine, &mut sink, 0, 50_000);
    assert_eq!(engine.stats().decode_errors, 1);
    assert_eq!(engine.stats().decoded, 0);
    assert_eq!(engine.stats().concealed, 1);
}

#[test]
fn mute_and_volume_apply_after_decode_without_stopping_sequence() {
    let packets = tones(8, 0);
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for packet in packets {
        push(&mut engine, packet, 0);
    }
    for now in (0..80_000).step_by(5000) {
        engine.render(&mut sink, now, 0.5, true).unwrap();
    }
    assert!(sink.output.iter().all(|s| *s == 0.0));
    assert_eq!(engine.stats().decoded, 4);
    render(&mut engine, &mut sink, 80_000, 120_000);
    assert_eq!(engine.stats().decoded, 8);
    assert!(sink.output.iter().any(|s| s.abs() > 0.1));
    let mut half = Engine::new().unwrap();
    let mut half_sink = Recorder::default();
    let mut full = Engine::new().unwrap();
    let mut full_sink = Recorder::default();
    for packet in tones(5, 0) {
        push(&mut half, packet.clone(), 0);
        push(&mut full, packet, 0);
    }
    for now in (0..90_000).step_by(5000) {
        half.render(&mut half_sink, now, 0.5, false).unwrap();
        full.render(&mut full_sink, now, 1.0, false).unwrap();
    }
    assert!((half.stats().output_rms * 2.0 - full.stats().output_rms).abs() < 0.00001);
}

#[test]
fn fake_device_errors_propagate_and_flush_cannot_leak_pcm() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    push(&mut engine, tones(1, 0).remove(0), 0);
    render(&mut engine, &mut sink, 0, 45_000);
    sink.fail_write = true;
    assert!(engine.render(&mut sink, 45_000, 1.0, false).is_err());
    assert_eq!(
        engine.stats().dropped,
        1,
        "failed device commit loses the partial source packet"
    );
    engine.reset();
    assert_eq!(
        engine.stats().dropped,
        1,
        "reset must not count failed output twice"
    );
    sink.fail_write = false;
    let start = sink.output.len();
    render(&mut engine, &mut sink, 50_000, 100_000);
    assert!(sink.output[start..].iter().all(|s| *s == 0.0));
    assert!(sink.clears > 0);
    sink.fake.fail_next = true;
    assert!(engine.render(&mut sink, 100_000, 1.0, false).is_err());
}

#[test]
fn reset_counts_pending_packets_and_one_partial_frame_once() {
    let mut engine = Engine::new().unwrap();
    let mut sink = Recorder::default();
    for packet in tones(3, 0) {
        push(&mut engine, packet, 0);
    }
    render(&mut engine, &mut sink, 0, 45_000);
    assert_eq!(engine.stats().decoded, 1);
    engine.reset();
    assert_eq!(
        engine.stats().dropped,
        3,
        "two queued packets plus one discarded PCM tail"
    );
    engine.reset();
    assert_eq!(
        engine.stats().dropped,
        3,
        "empty reset must not double count"
    );
}

#[test]
fn fake_sink_keeps_clock_phase_and_caps_scheduler_stall_catch_up() {
    let mut sink = FakeSink::default();
    assert_eq!(sink.writable(0).unwrap().0, 240);
    assert_eq!(sink.writable(5_200).unwrap().0, 240);
    assert_eq!(sink.writable(10_400).unwrap().0, 240);
    // Delayed wake crosses two retained deadlines instead of shifting clock.
    assert_eq!(sink.writable(20_500).unwrap().0, 480);
    assert_eq!(sink.writable(21_000).unwrap().0, 0);
    assert_eq!(sink.writable(1_000_000).unwrap().0, 2400);
}

#[test]
fn handle_epoch_controls_and_joined_shutdown_use_fake_only() {
    let player = AudioPlayer::new(OutputMode::Fake).unwrap();
    let handle = player.handle();
    handle.reset_epoch(7);
    let bytes = wire(&tones(1, 0).remove(0));
    assert!(!handle.push(6, &bytes, Instant::now()));
    assert!(handle.push(7, &bytes, Instant::now()));
    assert!(!handle.push(7, b"BPA1malformed", Instant::now()));
    handle.set_volume(0.3);
    handle.set_muted(true);
    let stats = handle.stats();
    assert_eq!(stats.epoch, 7);
    assert_eq!(stats.volume, 0.3);
    assert!(stats.muted);
    assert_eq!(stats.received, 2);
    assert_eq!(stats.rejected, 1);
    handle.reset_epoch(8);
    assert_eq!(handle.stats().depth_ms, 0.0);
    assert!(!handle.push(7, &bytes, Instant::now()));
    handle.set_volume(f32::NAN);
    assert_eq!(handle.stats().volume, 0.0);
    drop(player);
    assert!(!handle.push(8, &bytes, Instant::now()));
}
