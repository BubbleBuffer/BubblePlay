// Runs the log and feedback upload over a platform [CtlBridge]: one [ClientLogUploader] per
// authenticated connection. A new connection gets a new uploader (message ids restart, the
// host's `logs-state` gives the cursors again); a closed one disposes it.
//
// The uploader's transport is synchronous and the bridge's send is not, so a send the bridge
// refuses (false, or an error) is turned into the answer the uploader already knows how to
// handle: a `rate-limited` ack for a batch (same batch again later) and a `retry` ack for a
// feedback message (the report starts over, at most three times per connection).
//
// Nothing here throws into the caller and nothing here writes log lines (the queue would feed
// on its own chatter). The coordinator does not touch `AppLog.setContext`: the session code
// owns it, and the uploader only reads lines by PC id.

import 'dart:async';

import '../../feedback/outbox.dart';
import '../app_log.dart';
import 'client_log_uploader.dart';
import 'ctl_bridge.dart';
import 'log_upload_transport.dart';

class UploadCoordinator {
  UploadCoordinator({
    required this.bridge,
    required this.appLog,
    required Outbox outbox,
    required this.stream,
    this.ackTimeout = const Duration(seconds: 30),
    this.feedbackPollInterval = const Duration(seconds: 10),
    this.feedbackRetryDelay = const Duration(seconds: 5),
    this.sendRetryDelay = const Duration(seconds: 1),
  }) {
    _outbox = _ReportingOutbox(outbox, _reportSent);
  }

  final CtlBridge bridge;
  final LogLineSource appLog;

  /// The `src` of this process's lines (`AppLog.instance.src`: `app-win`, `app-android`).
  final String stream;
  final Duration ackTimeout;
  final Duration feedbackPollInterval;
  final Duration feedbackRetryDelay;

  /// How long a batch the bridge refused waits before it is sent again.
  final Duration sendRetryDelay;

  late final Outbox _outbox;
  final StreamController<String> _sent = StreamController<String>.broadcast();
  StreamSubscription<CtlEvent>? _sub;
  ClientLogUploader? _uploader;
  _BridgeTransport? _transport;
  bool _disposed = false;

  /// The ids of reports the PC has taken (removed from the outbox) while connected.
  Stream<String> get sentReports => _sent.stream;

  /// True while an uploader serves a connection.
  bool get isConnected => _uploader != null;

  /// The uploader of the current connection, if any (diagnostics and tests).
  ClientLogUploader? get uploader => _uploader;

  /// Starts listening to the bridge. Safe to call twice.
  void start() {
    try {
      if (_disposed || _sub != null) return;
      _sub = bridge.events.listen(
        _onEvent,
        onError: (Object _) {},
        onDone: _closeConnection,
        cancelOnError: false,
      );
    } catch (_) {}
  }

  /// A report was just saved: send it now when connected instead of at the next poll.
  void checkFeedback() {
    try {
      _uploader?.checkFeedback();
    } catch (_) {}
  }

  /// Stops listening and disposes the current uploader. The coordinator cannot be restarted.
  void dispose() {
    if (_disposed) return;
    _disposed = true;
    _closeConnection();
    final sub = _sub;
    _sub = null;
    try {
      unawaited(sub?.cancel() ?? Future<void>.value());
    } catch (_) {}
    try {
      unawaited(_sent.close());
    } catch (_) {}
  }

  void _reportSent(String id) {
    if (!_disposed && !_sent.isClosed) _sent.add(id);
  }

  void _onEvent(CtlEvent event) {
    try {
      if (_disposed) return;
      switch (event) {
        case CtlOpened():
          _closeConnection();
          if (!event.peerCaps.contains(clientLogsCapability)) return;
          _open(event.pcId);
        case CtlMessage():
          _transport?.deliver(event.json);
        case CtlClosed():
          _closeConnection();
      }
    } catch (_) {}
  }

  void _open(String pcId) {
    final transport = _BridgeTransport(bridge, stream, sendRetryDelay);
    final uploader = ClientLogUploader(
      transport: transport,
      appLog: appLog,
      outbox: _outbox,
      stream: stream,
      pcId: pcId,
      ackTimeout: ackTimeout,
      feedbackPollInterval: feedbackPollInterval,
      feedbackRetryDelay: feedbackRetryDelay,
    );
    _transport = transport;
    _uploader = uploader;
    uploader.start();
  }

  void _closeConnection() {
    final uploader = _uploader;
    final transport = _transport;
    _uploader = null;
    _transport = null;
    try {
      uploader?.dispose();
    } catch (_) {}
    try {
      transport?.close();
    } catch (_) {}
  }
}

/// The outbox the uploader sees: the same files, plus a notice when a report is removed.
class _ReportingOutbox extends Outbox {
  _ReportingOutbox(Outbox inner, this._onSent) : super(inner.dir);

  final void Function(String id) _onSent;

  @override
  Future<void> markSent(String id) async {
    await super.markSent(id);
    try {
      _onSent(id);
    } catch (_) {}
  }
}

/// [LogUploadTransport] over a [CtlBridge] for one connection.
class _BridgeTransport implements LogUploadTransport {
  _BridgeTransport(this._bridge, this._stream, this._retryDelay);

  final CtlBridge _bridge;
  final String _stream;
  final Duration _retryDelay;
  final StreamController<Map<String, Object?>> _in = StreamController<Map<String, Object?>>();
  final Set<String> _failedFeedback = {};
  Future<void> _chain = Future<void>.value();
  bool _closed = false;

  @override
  Stream<Map<String, Object?>> get messages => _in.stream;

  @override
  int get bufferedBytes => _bridge.bufferedBytes;

  @override
  Duration get sinceLastInput => _bridge.sinceLastInput;

  void deliver(Map<String, dynamic> json) {
    if (_closed || _in.isClosed) return;
    _in.add(Map<String, Object?>.of(json));
  }

  void close() {
    if (_closed) return;
    _closed = true;
    try {
      unawaited(_in.close());
    } catch (_) {}
  }

  @override
  void send(Map<String, Object?> message) {
    if (_closed) throw StateError('connection closed');
    final type = message['type'];
    final id = message['id'];
    if (type == 'feedback-begin' && id is String) _failedFeedback.remove(id);
    if (type == 'feedback-chunk' && id is String && _failedFeedback.contains(id)) return;
    // Messages leave one at a time, in order.
    _chain = _chain.then((_) => _dispatch(message));
  }

  Future<void> _dispatch(Map<String, Object?> message) async {
    if (_closed) return;
    final queuedId = message['id'];
    if (message['type'] == 'feedback-chunk' && queuedId is String && _failedFeedback.contains(queuedId)) {
      return; // an earlier message of this report was refused while this one waited
    }
    var ok = false;
    try {
      ok = await _bridge.send(Map<String, dynamic>.of(message));
    } catch (_) {}
    if (ok || _closed) return;
    final type = message['type'];
    final id = message['id'];
    if (type == 'logs-batch') {
      deliver({
        'type': 'logs-ack',
        'v': 1,
        'stream': _stream,
        'id': id,
        'status': 'rate-limited',
        'retryAfterMs': _retryDelay.inMilliseconds,
      });
    } else if ((type == 'feedback-begin' || type == 'feedback-chunk') && id is String) {
      _failedFeedback.add(id);
      deliver({'type': 'feedback-ack', 'v': 1, 'id': id, 'status': 'retry'});
    }
  }
}
