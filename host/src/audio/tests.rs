use super::*;
use std::sync::atomic::AtomicUsize;

fn frame(seq: u32) -> AudioFrame {
    AudioFrame {
        seq,
        capture_ts_us: seq as u64,
        discontinuity: false,
        samples: FRAME_SAMPLES,
        payload: vec![0xFC],
    }
}

fn pipeline() -> (Pipeline, Arc<Mutex<Pending>>) {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let sink = Arc::new(Mutex::new(None));
    let activity = Activity::new().expect("activity");
    let pipe = Pipeline::new(
        Instant::now(),
        activity,
        Arc::clone(&pending),
        sink,
        FRAME_SAMPLES,
    );
    (pipe, pending)
}

fn take(pending: &Arc<Mutex<Pending>>) -> Vec<AudioFrame> {
    lock(pending).frames.drain(..).collect()
}

/// Pushes one 10 ms frame and returns what reached the drain queue.
fn push_frame(
    pipe: &mut Pipeline,
    pending: &Arc<Mutex<Pending>>,
    pcm: &[[f32; 2]],
    now: Instant,
) -> Vec<AudioFrame> {
    pipe.push_stereo(pcm, now);
    take(pending)
}

fn tone() -> Vec<[f32; 2]> {
    (0..FRAME_SAMPLES as usize)
        .map(|i| {
            let v = (i as f32 * 0.1).sin() * 0.3;
            [v, v]
        })
        .collect()
}

#[test]
fn pending_buffer_drops_oldest_and_marks_the_new_front() {
    let mut pending = Pending::default();
    for seq in 0..(PENDING_CAPACITY as u32 + 3) {
        pending.push(frame(seq));
    }
    assert_eq!(pending.frames.len(), PENDING_CAPACITY);
    let front = pending.frames.front().unwrap();
    assert_eq!(front.seq, 3);
    assert!(front.discontinuity);
    assert!(pending.frames.iter().skip(1).all(|f| !f.discontinuity));
}

#[test]
fn silence_is_gated_end_to_end_into_packets() {
    let (mut pipe, pending) = pipeline();
    let now = Instant::now();
    // One frame at a time: the drain queue holds 10, so a batch would overflow it.
    let mut sent = Vec::new();
    for _ in 0..40 {
        sent.extend(push_frame(&mut pipe, &pending, &SILENCE, now));
    }
    // The first 20 silent frames (200 ms) are sent, the rest suppressed.
    assert_eq!(sent.len(), 20);
    assert!(sent[0].discontinuity);
    assert!(sent[1..].iter().all(|f| !f.discontinuity));
    for (i, f) in sent.iter().enumerate() {
        assert_eq!(f.seq, i as u32, "seq counts sent packets only");
    }

    // Sound returns: the first packet after the suppressed span is marked.
    let resumed = push_frame(&mut pipe, &pending, &tone(), now);
    assert_eq!(resumed.len(), 1);
    assert!(resumed[0].discontinuity);
    assert_eq!(resumed[0].seq, 20);
}

#[test]
fn sink_receives_every_packet_and_the_drain_queue_stays_empty() {
    let got: Arc<Mutex<Vec<AudioFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_got = Arc::clone(&got);
    let sink: AudioSink = Arc::new(move |f: AudioFrame| {
        lock(&sink_got).push(f);
    });
    let pending = Arc::new(Mutex::new(Pending::default()));
    let mut pipe = Pipeline::new(
        Instant::now(),
        Activity::new().expect("activity"),
        Arc::clone(&pending),
        Arc::new(Mutex::new(Some(sink))),
        FRAME_SAMPLES,
    );
    let now = Instant::now();
    // 50 packets is well past the drain queue's capacity of 10.
    for _ in 0..50 {
        pipe.push_stereo(&tone(), now);
    }
    assert!(
        take(&pending).is_empty(),
        "fallback queue unused with a sink"
    );
    let received = lock(&got);
    assert_eq!(received.len(), 50, "sink gets every packet");
    for (i, f) in received.iter().enumerate() {
        assert_eq!(f.seq, i as u32);
    }
    assert!(received[0].discontinuity);
    assert!(received[1..].iter().all(|f| !f.discontinuity));
}

#[test]
fn every_emitted_packet_is_a_valid_bpa1_packet_with_a_480_sample_opus_frame() {
    let (mut pipe, pending) = pipeline();
    let now = Instant::now();
    let sine: Vec<[f32; 2]> = (0..FRAME_SAMPLES as usize * 5)
        .map(|i| {
            let v = (i as f32 * 0.05).sin() * 0.5;
            [v, -v]
        })
        .collect();
    pipe.push_stereo(&sine, now);
    let sent = take(&pending);
    assert_eq!(sent.len(), 5);
    for f in &sent {
        let bytes = encode_packet(f).expect("encodable");
        let back = decode_packet(&bytes).expect("decodable");
        assert_eq!(back.payload, f.payload);
        let samples = opus::packet::get_nb_samples(&f.payload, 48_000).expect("opus packet");
        assert_eq!(samples, FRAME_SAMPLES as usize);
    }
}

#[test]
fn a_20_ms_pipeline_emits_960_sample_packets_and_keeps_the_silence_window() {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let mut pipe = Pipeline::new(
        Instant::now(),
        Activity::new().expect("activity"),
        Arc::clone(&pending),
        Arc::new(Mutex::new(None)),
        FRAME_SAMPLES_20MS,
    );
    let now = Instant::now();
    let sine: Vec<[f32; 2]> = (0..FRAME_SAMPLES_20MS as usize * 3)
        .map(|i| {
            let v = (i as f32 * 0.05).sin() * 0.5;
            [v, -v]
        })
        .collect();
    pipe.push_stereo(&sine, now);
    let sent = take(&pending);
    assert_eq!(sent.len(), 3, "one packet per 20 ms of audio");
    for f in &sent {
        assert_eq!(f.samples, FRAME_SAMPLES_20MS);
        let back = decode_packet(&encode_packet(f).expect("encodable")).expect("decodable");
        assert_eq!(back.samples, FRAME_SAMPLES_20MS);
        let samples = opus::packet::get_nb_samples(&f.payload, 48_000).expect("opus packet");
        assert_eq!(
            samples, FRAME_SAMPLES_20MS as usize,
            "an Opus frame of 20 ms"
        );
    }
    // Digital silence is still sent for about 200 ms (10 packets of 20 ms), then suppressed.
    let silence = vec![[0.0f32; 2]; FRAME_SAMPLES_20MS as usize * 30];
    pipe.push_stereo(&silence, now);
    assert_eq!(take(&pending).len(), 10);
}

#[test]
fn gap_marks_the_next_packet_and_drops_partial_audio() {
    let (mut pipe, pending) = pipeline();
    let now = Instant::now();
    let loud = vec![[0.2f32, 0.2]; FRAME_SAMPLES as usize];
    pipe.push_stereo(&loud, now);
    pipe.push_stereo(&vec![[0.2f32, 0.2]; 100], now);
    pipe.gap();
    pipe.push_stereo(&vec![[0.2f32, 0.2]; FRAME_SAMPLES as usize], now);
    let sent = take(&pending);
    assert_eq!(
        sent.len(),
        2,
        "the 100 partial frames were dropped by the gap"
    );
    assert!(sent[0].discontinuity);
    assert!(sent[1].discontinuity);
}

#[test]
fn capture_timestamps_follow_the_supplied_clock_origin() {
    let pending = Arc::new(Mutex::new(Pending::default()));
    let origin = Instant::now();
    let sink = Arc::new(Mutex::new(None));
    let mut pipe = Pipeline::new(
        origin,
        Activity::new().unwrap(),
        Arc::clone(&pending),
        sink,
        FRAME_SAMPLES,
    );
    let later = origin + Duration::from_millis(250);
    pipe.push_stereo(&vec![[0.3f32, 0.3]; FRAME_SAMPLES as usize], later);
    let sent = take(&pending);
    assert_eq!(sent[0].capture_ts_us, 250_000);
}

static OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn refusing_encoder() -> Result<OpusEncoder> {
    OPEN_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    anyhow::bail!("encoder refused for the test")
}

#[test]
fn failing_encoder_creation_is_retried_at_most_once_per_second() {
    let (mut pipe, pending) = pipeline();
    pipe.open_encoder = refusing_encoder;
    let now = Instant::now();
    // 100 active frames arrive well inside one second, so one attempt is expected.
    for _ in 0..100 {
        pipe.push_stereo(&tone(), now);
    }
    assert_eq!(OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
    assert!(take(&pending).is_empty(), "no packets without an encoder");
}

static QUIET_OPEN_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn refusing_quiet_encoder() -> Result<OpusEncoder> {
    QUIET_OPEN_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    anyhow::bail!("encoder refused during silence")
}

#[test]
fn device_checks_retry_encoder_after_silence_suppresses_frames() {
    let (mut pipe, pending) = pipeline();
    pipe.open_encoder = refusing_quiet_encoder;
    let now = Instant::now();
    for _ in 0..30 {
        pipe.push_stereo(&SILENCE, now);
    }
    assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
    let attempted_at = pipe.encoder_attempt.unwrap();
    assert!(!pipe.ensure_encoder(attempted_at + Duration::from_millis(999)));
    assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 1);
    assert!(!pipe.ensure_encoder(attempted_at + REOPEN_INTERVAL));
    assert_eq!(QUIET_OPEN_ATTEMPTS.load(Ordering::SeqCst), 2);
    assert!(take(&pending).is_empty());
}

#[test]
fn filler_only_owes_silence_for_quiet_time_past_fill_after() {
    let t0 = Instant::now();
    let (mut pipe, _pending) = pipeline();
    let mut filler = Filler::new(t0);
    filler.fill(t0 + Duration::from_millis(40), &mut pipe);
    assert_eq!(
        filler.covered_until, t0,
        "under FILL_AFTER nothing is generated"
    );
    filler.fill(t0 + Duration::from_millis(60), &mut pipe);
    assert_eq!(filler.covered_until, t0 + Duration::from_millis(60));
}

#[test]
fn filler_reset_leaves_no_catch_up_burst_after_a_gap() {
    let t0 = Instant::now();
    let (mut pipe, pending) = pipeline();
    let mut filler = Filler::new(t0);
    // No device for five seconds: the filler is reset every pass, not left behind.
    let resumed = t0 + Duration::from_secs(5);
    filler.reset(resumed);
    filler.fill(resumed, &mut pipe);
    assert_eq!(
        filler.covered_until, resumed,
        "nothing owed right after the reset"
    );
    assert!(take(&pending).is_empty());
}

#[test]
fn filler_catch_up_is_capped_per_pass() {
    let t0 = Instant::now();
    let (mut pipe, _pending) = pipeline();
    let mut filler = Filler::new(t0);
    filler.fill(t0 + Duration::from_secs(5), &mut pipe);
    assert_eq!(
        filler.covered_until,
        t0 + FRAME_DURATION * MAX_FILL_FRAMES_PER_PASS as u32
    );
}
