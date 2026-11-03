import 'dart:async';
import 'dart:convert';

import '../../backend/pigeon.g.dart';
import 'ctl_bridge.dart';

/// The Android bridge: pigeon to the Kotlin shell (`CtlShell`), which follows the active
/// session's control channel. Events come in through [CtlFlutterApi.ctlEvent]; sends go out
/// through [CtlHostApi.ctlSend]. [bufferedBytes] and [sinceLastInput] come from `ctlState`,
/// polled every [pollInterval] only while a connection is open.
class AndroidCtlBridge implements CtlBridge {
  AndroidCtlBridge({
    CtlHostApi? host,
    this.pollInterval = const Duration(milliseconds: 250),
  }) : _host = host ?? CtlHostApi() {
    _receiver = _Receiver(this);
  }

  final CtlHostApi _host;

  /// How often `ctlState` is read while connected.
  final Duration pollInterval;

  /// The most "time since input" Dart will report; the native side sends the largest int64
  /// when there was no input yet.
  static const Duration _longAgo = Duration(hours: 1);

  late final _Receiver _receiver;
  final StreamController<CtlEvent> _events = StreamController<CtlEvent>();
  final Stopwatch _clock = Stopwatch()..start();
  Timer? _timer;
  bool _open = false;
  bool _polling = false;
  int _buffered = 0;
  Duration _sinceInput = _longAgo;
  Duration _stateAt = Duration.zero;

  /// The receiver to register with `CtlFlutterApi.setUp`.
  CtlFlutterApi get receiver => _receiver;

  @override
  Stream<CtlEvent> get events => _events.stream;

  @override
  Future<bool> send(Map<String, dynamic> message) async {
    if (!_open) return false;
    try {
      return await _host.ctlSend(jsonEncode(message));
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

  /// Stops polling and ends the event stream.
  void dispose() {
    _timer?.cancel();
    _timer = null;
    _open = false;
    if (!_events.isClosed) _events.close();
  }

  void _onEvent(String kind, String json) {
    if (_events.isClosed) return;
    try {
      switch (kind) {
        case 'opened':
          final map = jsonDecode(json) as Map<String, dynamic>;
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
          _events.add(CtlMessage(jsonDecode(json) as Map<String, dynamic>));
        case 'closed':
          if (!_open) return;
          _open = false;
          _stopPolling();
          _events.add(const CtlClosed());
      }
    } catch (_) {
      // A malformed event is dropped; the bridge never throws into the platform channel.
    }
  }

  void _startPolling() {
    _timer?.cancel();
    unawaited(_poll());
    _timer = Timer.periodic(pollInterval, (_) => unawaited(_poll()));
  }

  void _stopPolling() {
    _timer?.cancel();
    _timer = null;
  }

  Future<void> _poll() async {
    if (_polling || !_open) return;
    _polling = true;
    try {
      final state = await _host.ctlState();
      if (!_open) return;
      _buffered = state.bufferedBytes;
      final ms = state.sinceLastInputMs;
      _sinceInput = ms >= _longAgo.inMilliseconds
          ? _longAgo
          : Duration(milliseconds: ms < 0 ? 0 : ms);
      _stateAt = _clock.elapsed;
      if (!state.open) {
        // The shell says there is no connection (its `closed` event may still be on the way).
        _open = false;
        _stopPolling();
        _events.add(const CtlClosed());
      }
    } catch (_) {
      // Keep the last numbers; the next poll tries again.
    } finally {
      _polling = false;
    }
  }
}

class _Receiver implements CtlFlutterApi {
  _Receiver(this._bridge);

  final AndroidCtlBridge _bridge;

  @override
  void ctlEvent(String kind, String json) => _bridge._onEvent(kind, json);
}

/// The Android bridge, registered with the pigeon callback.
CtlBridge? createAndroidCtlBridge() {
  final bridge = AndroidCtlBridge();
  CtlFlutterApi.setUp(bridge.receiver);
  return bridge;
}
