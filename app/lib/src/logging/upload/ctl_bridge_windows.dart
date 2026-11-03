// ignore_for_file: prefer_initializing_formals
import 'dart:async';
import 'dart:convert';

import '../../rust/api.dart' as api;
import 'ctl_bridge.dart';

/// The Windows bridge: flutter_rust_bridge to the Rust core's ctl hook (`ctl_log_*` in
/// `app/rust/src/api.rs`). The core pushes one JSON string per event through [events]
/// (`opened`, `message`, `closed`); sends go out through [send]; [bufferedBytes] and
/// [sinceLastInput] come from [state], polled every [pollInterval] only while a connection
/// is open and aged by a stopwatch in between, like the Android bridge.
class WindowsCtlBridge implements CtlBridge {
  WindowsCtlBridge({
    required Stream<String> Function() events,
    required bool Function(String json) send,
    required api.CtlLogState Function() state,
    this.pollInterval = const Duration(milliseconds: 250),
  })  : _send = send,
        _state = state {
    _sub = events().listen(_onEvent, onError: (Object _) {});
  }

  /// The bridge over the generated binding (`RustLib` must be initialised).
  factory WindowsCtlBridge.generated() => WindowsCtlBridge(
        events: api.ctlLogEvents,
        send: (json) => api.ctlLogSend(json: json),
        state: api.ctlLogState,
      );

  final bool Function(String json) _send;
  final api.CtlLogState Function() _state;

  /// How often the state is read while connected.
  final Duration pollInterval;

  /// The most "time since input" Dart reports; the core sends the largest int64 when there
  /// was no input yet.
  static const Duration _longAgo = Duration(hours: 1);

  late final StreamSubscription<String> _sub;
  final StreamController<CtlEvent> _events = StreamController<CtlEvent>();
  final Stopwatch _clock = Stopwatch()..start();
  Timer? _timer;
  bool _open = false;
  int _buffered = 0;
  Duration _sinceInput = _longAgo;
  Duration _stateAt = Duration.zero;

  @override
  Stream<CtlEvent> get events => _events.stream;

  @override
  Future<bool> send(Map<String, dynamic> message) async {
    if (!_open) return false;
    try {
      return _send(jsonEncode(message));
    } catch (_) {
      return false;
    }
  }

  @override
  int get bufferedBytes => _open ? _buffered : 0;

  @override
  Duration get sinceLastInput {
    if (!_open) return _longAgo;
    final aged = _sinceInput + (_clock.elapsed - _stateAt);
    return aged > _longAgo ? _longAgo : aged;
  }

  /// Stops polling and the event subscription, and ends the event stream.
  void dispose() {
    _timer?.cancel();
    _timer = null;
    _open = false;
    unawaited(_sub.cancel());
    if (!_events.isClosed) unawaited(_events.close());
  }

  void _onEvent(String line) {
    if (_events.isClosed) return;
    try {
      final map = jsonDecode(line) as Map<String, dynamic>;
      switch (map['kind']) {
        case 'opened':
          final caps = (map['peerCaps'] as List<dynamic>? ?? const <dynamic>[])
              .whereType<String>()
              .toSet();
          _open = true;
          _buffered = 0;
          _sinceInput = _longAgo;
          _stateAt = _clock.elapsed;
          _events.add(CtlOpened(map['pcId'] as String? ?? '', caps));
          _startPolling();
        case 'message':
          if (!_open) return;
          _events.add(CtlMessage(map['json'] as Map<String, dynamic>));
        case 'closed':
          if (!_open) return;
          _open = false;
          _stopPolling();
          _events.add(const CtlClosed());
      }
    } catch (_) {
      // A malformed event is dropped; the bridge never throws into the binding.
    }
  }

  void _startPolling() {
    _timer?.cancel();
    _poll();
    _timer = Timer.periodic(pollInterval, (_) => _poll());
  }

  void _stopPolling() {
    _timer?.cancel();
    _timer = null;
  }

  void _poll() {
    if (!_open) return;
    try {
      final state = _state();
      _buffered = state.bufferedBytes;
      final ms = state.sinceLastInputMs;
      _sinceInput = ms >= _longAgo.inMilliseconds
          ? _longAgo
          : Duration(milliseconds: ms < 0 ? 0 : ms);
      _stateAt = _clock.elapsed;
      if (!state.open) {
        // The core says there is no connection (its `closed` event may still be on the way).
        _open = false;
        _stopPolling();
        _events.add(const CtlClosed());
      }
    } catch (_) {
      // Keep the last numbers; the next poll tries again.
    }
  }
}

/// The Windows bridge over the generated binding, or null when the core is not loaded (tests,
/// a failed `RustLib.init`). Never throws.
CtlBridge? createWindowsCtlBridge() {
  try {
    return WindowsCtlBridge.generated();
  } catch (_) {
    return null;
  }
}
