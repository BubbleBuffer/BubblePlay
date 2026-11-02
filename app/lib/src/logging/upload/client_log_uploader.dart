// The client side of the log and feedback upload (docs/architecture/client-log-upload.md).
//
// One [ClientLogUploader] serves one authenticated connection to one PC and one stream (the
// `src` of the lines this process owns). [start] waits for `logs-state`, which carries the
// cursors, then sends history (oldest first) and afterwards live lines, one batch in flight,
// and finally queued feedback reports. It never hurts the stream: a message goes out only when
// the control channel's send buffer is small, no input was sent just now, and at most one
// message per `minBatchIntervalMs`.
//
// Cursors: `<ts>#<nnnnnn>` (fixed width, see LogPage). Lines of this PC and lines that relate
// to no PC are merged by `ts`; when several lines share a millisecond the PC's lines come
// first, then the misc lines, each in file order. Only lines whose `src` is this stream are
// sent (the Kotlin core's files share the directory and upload on their own stream).
//
// Nothing here throws into the caller, and nothing here writes log lines (that would feed the
// queue with its own chatter).

import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import '../../feedback/outbox.dart';
import '../app_log.dart';
import 'log_upload_transport.dart';

/// The cursor sent as `from` for a stream that has never uploaded. It is a valid cursor and
/// sorts below every timestamp-based cursor.
const firstCursor = '0';

const _maxBatchBytes = 16384;
const _maxBatchLines = 100;
const _maxFeedbackPngBytes = 5 * 1024 * 1024;
const _maxFeedbackChunkBytes = 32768;
const _bufferLimit = 16384;
const _inputQuiet = Duration(milliseconds: 250);
const _scanPage = 500;
const _scanPages = 40;

final RegExp _cursorShape = RegExp(r'^[0-9A-Za-z#:._-]{1,64}$');

class ClientLogUploader {
  ClientLogUploader({
    required this.transport,
    required this.appLog,
    required this.outbox,
    required this.stream,
    required this.pcId,
    this.sendFeedback = true,
    this.ackTimeout = const Duration(seconds: 30),
    this.feedbackPollInterval = const Duration(seconds: 10),
    this.feedbackRetryDelay = const Duration(seconds: 5),
  });

  final LogUploadTransport transport;
  final LogLineSource appLog;
  final Outbox outbox;

  /// The `src` of the lines this uploader owns: `app-win`, `app-android`, ...
  final String stream;

  /// The PC this connection goes to.
  final String pcId;

  /// Whether this uploader also sends the outbox (one uploader per connection should).
  final bool sendFeedback;

  /// No ack for this long means the connection is dead: the uploader stops.
  final Duration ackTimeout;
  final Duration feedbackPollInterval;
  final Duration feedbackRetryDelay;

  StreamSubscription<Map<String, Object?>>? _sub;
  bool _started = false;
  bool _disposed = false;
  bool _stopped = false;
  String? _stopReason;

  // Limits (from logs-state, never above the wire doc's own).
  bool _ready = false;
  int _maxBytes = _maxBatchBytes;
  int _maxLines = _maxBatchLines;
  Duration _minInterval = const Duration(milliseconds: 500);

  /// The last cursor the host holds (or that a rejected batch skipped past).
  String? _cursor;
  int _nextId = 1;
  _Batch? _batch;
  bool _caughtUp = false;
  int? _idleEpoch;

  _Feedback? _fb;
  final List<OutboxItem> _fbQueue = [];
  final Set<String> _fbDone = {};
  final Set<String> _fbBlocked = {};
  final Map<String, int> _fbTries = {};
  bool _fbCheckDue = true;

  Timer? _cooldown;
  Timer? _wake;
  Timer? _ackTimer;
  Timer? _retryTimer;
  Timer? _fbTimer;
  Timer? _fbRetryTimer;

  bool _stepping = false;
  bool _again = false;

  /// True once the uploader stopped by itself (budget, dead connection, send failure).
  bool get isStopped => _stopped;

  /// Why it stopped, for diagnostics.
  String? get stopReason => _stopReason;

  /// The last `reason` of a `rejected` batch, if any.
  String? lastRejectReason;

  /// The cursor the host last confirmed (null: nothing confirmed yet).
  String? get cursor => _cursor;

  /// Listens for the host's messages. Call on an authenticated connection.
  void start() {
    try {
      if (_started || _disposed) return;
      _started = true;
      _sub = transport.messages.listen(
        _onMessage,
        onError: (Object _) {},
        onDone: () => _stop('connection closed'),
        cancelOnError: false,
      );
    } catch (_) {
      _stop('start failed');
    }
  }

  /// Asks for an outbox check now (a report was just queued).
  void checkFeedback() {
    if (_disposed || _stopped || !sendFeedback) return;
    _fbCheckDue = true;
    _pump();
  }

  /// Cancels every timer and the subscription. The uploader cannot be restarted.
  void dispose() {
    _disposed = true;
    _cancelAll();
    final sub = _sub;
    _sub = null;
    try {
      unawaited(sub?.cancel() ?? Future<void>.value());
    } catch (_) {}
  }

  // ---- messages from the host ----------------------------------------------------------

  void _onMessage(Map<String, Object?> m) {
    try {
      if (_disposed || _stopped) return;
      if (m['v'] != 1) return;
      switch (m['type']) {
        case 'logs-state':
          _onState(m);
        case 'logs-ack':
          _onLogsAck(m);
        case 'feedback-ack':
          _onFeedbackAck(m);
      }
    } catch (_) {}
  }

  void _onState(Map<String, Object?> m) {
    if (_ready) return;
    final limits = m['limits'];
    if (limits is Map) {
      _maxBytes = _clampInt(limits['maxBatchBytes'], _maxBatchBytes, 512, _maxBatchBytes);
      _maxLines = _clampInt(limits['maxBatchLines'], _maxBatchLines, 1, _maxBatchLines);
      _minInterval = Duration(milliseconds: _clampInt(limits['minBatchIntervalMs'], 500, 0, 60000));
    }
    final cursors = m['cursors'];
    if (cursors is Map) {
      final c = cursors[stream];
      if (c is String && _cursorShape.hasMatch(c)) _cursor = c;
    }
    _ready = true;
    if (sendFeedback && feedbackPollInterval > Duration.zero) {
      _fbTimer = Timer.periodic(feedbackPollInterval, (_) {
        _fbCheckDue = true;
        _pump();
      });
    }
    _pump();
  }

  void _onLogsAck(Map<String, Object?> m) {
    final b = _batch;
    if (b == null || !b.sent || m['stream'] != stream || m['id'] != b.id) return;
    _ackTimer?.cancel();
    switch (m['status']) {
      case 'ok':
        final c = m['cursor'];
        _cursor = c is String && _cursorShape.hasMatch(c) ? c : b.to;
        _batch = null;
        _pump();
      case 'rate-limited':
        final after = _clampInt(m['retryAfterMs'], 1000, 0, 5 * 60 * 1000);
        b.sent = false;
        b.waitingRetry = true;
        _retryTimer?.cancel();
        _retryTimer = Timer(Duration(milliseconds: after), () {
          _retryTimer = null;
          b.waitingRetry = false;
          _pump();
        });
      case 'budget':
        _stop('budget');
      default:
        // rejected (or a status this version does not know): never retry this batch.
        final r = m['reason'];
        lastRejectReason = r is String ? r : '${m['status']}';
        _cursor = b.to;
        _batch = null;
        _pump();
    }
  }

  void _onFeedbackAck(Map<String, Object?> m) {
    final f = _fb;
    if (f == null || !f.awaitingAck || m['id'] != f.id) return;
    _ackTimer?.cancel();
    switch (m['status']) {
      case 'ok':
        _fbDone.add(f.id);
        unawaited(_finishFeedback(f));
      case 'retry':
        _fb = null;
        final tries = (_fbTries[f.id] ?? 0) + 1;
        _fbTries[f.id] = tries;
        _fbBlocked.add(f.id);
        if (tries <= 3) {
          _fbRetryTimer?.cancel();
          _fbRetryTimer = Timer(feedbackRetryDelay, () {
            _fbRetryTimer = null;
            _fbBlocked.remove(f.id);
            _fbCheckDue = true;
            _pump();
          });
        }
        _pump();
      default:
        // rejected: leave it in the outbox, try again on the next connection.
        _fb = null;
        _fbBlocked.add(f.id);
        _pump();
    }
  }

  Future<void> _finishFeedback(_Feedback f) async {
    try {
      await outbox.markSent(f.id);
    } catch (_) {}
    if (identical(_fb, f)) _fb = null;
    _pump();
  }

  // ---- the send loop -------------------------------------------------------------------

  bool get _canRun => !_disposed && !_stopped && _ready;

  void _pump() {
    if (!_canRun) return;
    if (_stepping) {
      _again = true;
      return;
    }
    unawaited(_loop());
  }

  Future<void> _loop() async {
    if (_stepping) return;
    _stepping = true;
    try {
      do {
        _again = false;
        await _stepOnce();
      } while (_again && _canRun);
    } catch (_) {
      _stop('internal error');
    } finally {
      _stepping = false;
    }
  }

  Future<void> _stepOnce() async {
    if (!_canRun || _cooldown != null) return;
    final b = _batch;
    final f = _fb;
    if (b != null && (b.sent || b.waitingRetry)) return; // waiting for an ack or a retry
    if (f != null && f.awaitingAck) return;

    Map<String, Object?> msg;
    void Function() onSent;
    if (b != null) {
      msg = b.msg;
      onSent = () {
        b.sent = true;
        _armAck();
      };
    } else if (f != null) {
      final step = f.next();
      msg = step.msg;
      onSent = () {
        f.advance();
        if (f.awaitingAck) _armAck();
      };
    } else {
      if (sendFeedback && _fbCheckDue) {
        _fbCheckDue = false;
        await _loadFeedback();
        if (!_canRun) return;
      }
      final nf = await _nextFeedback();
      if (!_canRun) return;
      if (nf != null) {
        _fb = nf;
        _again = true;
        return;
      }
      if (_caughtUp && appLog.writeEpoch == _idleEpoch) {
        _schedule(_minInterval);
        return;
      }
      final epoch = appLog.writeEpoch;
      final nb = await _prepareBatch();
      if (!_canRun) return;
      if (nb == null) {
        _caughtUp = true;
        _idleEpoch = epoch;
        _schedule(_minInterval);
        return;
      }
      _batch = nb;
      _again = true;
      return;
    }

    if (!_gateOpen()) return;
    try {
      transport.send(msg);
    } catch (_) {
      _stop('send failed');
      return;
    }
    onSent();
    _cooldown = Timer(_minInterval, () {
      _cooldown = null;
      _pump();
    });
  }

  /// The priority rules: small send buffer, no recent input. Schedules a retry when closed.
  bool _gateOpen() {
    int buffered;
    Duration since;
    try {
      buffered = transport.bufferedBytes;
      since = transport.sinceLastInput;
    } catch (_) {
      _stop('transport failed');
      return false;
    }
    if (buffered > _bufferLimit) {
      _schedule(const Duration(milliseconds: 100));
      return false;
    }
    if (since < _inputQuiet) {
      _schedule(_inputQuiet - since + const Duration(milliseconds: 5));
      return false;
    }
    return true;
  }

  void _schedule(Duration d) {
    _wake?.cancel();
    if (d < const Duration(milliseconds: 10)) d = const Duration(milliseconds: 10);
    _wake = Timer(d, () {
      _wake = null;
      _pump();
    });
  }

  void _armAck() {
    _ackTimer?.cancel();
    _ackTimer = Timer(ackTimeout, () => _stop('ack timeout'));
  }

  // ---- log batches ---------------------------------------------------------------------

  Future<_Batch?> _prepareBatch() async {
    final cur = _Cur.parse(_cursor);
    final fetched = await _fetch(cur);
    if (!_canRun) return null;
    final lines = <String>[];
    String? to;
    final id = _nextId;
    final from = _cursor ?? firstCursor;
    final base = jsonEncode({
      'type': 'logs-batch',
      'v': 1,
      'stream': stream,
      'id': id,
      'from': from,
      'to': 'x' * 64,
      'live': false,
      'lines': <String>[],
    });
    var bytes = utf8.encode(base).length;
    var taken = 0;
    for (final e in fetched.entries) {
      if (lines.length >= _maxLines) break;
      final add = utf8.encode(jsonEncode(e.raw)).length + 1;
      if (lines.isNotEmpty && bytes + add > _maxBytes) break;
      bytes += add;
      lines.add(e.raw);
      to = e.cursor;
      taken++;
    }
    if (lines.isEmpty || to == null) return null;
    final more = taken < fetched.entries.length || fetched.more;
    final live = _caughtUp && !more;
    _nextId++;
    if (!more) _caughtUp = true;
    return _Batch(
      id,
      to,
      {
        'type': 'logs-batch',
        'v': 1,
        'stream': stream,
        'id': id,
        'from': from,
        'to': to,
        'live': live,
        'lines': lines,
      },
    );
  }

  /// Lines of this stream after [cur], oldest first, both sources merged. When the scan hit
  /// its limit only the part that is complete for both sources is returned and [more] is set.
  Future<_Fetched> _fetch(_Cur? cur) async {
    final after = cur == null ? null : '${cur.ts}#000000';
    final entries = <_Entry>[];
    var more = false;
    String? horizon;

    Future<void> scan(int srcIdx) async {
      var pageAfter = after;
      var order = 0;
      for (var i = 0; i < _scanPages; i++) {
        final page = srcIdx == 0
            ? await appLog.linesFor(pcId, afterCursor: pageAfter, limit: _scanPage)
            : await appLog.miscLines(afterCursor: pageAfter, limit: _scanPage);
        String? lastTs;
        for (final raw in page.lines) {
          final json = _decode(raw);
          if (json == null) continue;
          final ts = json['ts'];
          if (ts is! String) continue;
          lastTs = ts;
          if (json['src'] == stream) entries.add(_Entry(raw, ts, srcIdx, order++));
        }
        if (!page.more || page.lines.isEmpty) return;
        pageAfter = page.cursor;
        if (i == _scanPages - 1 && lastTs != null) {
          more = true;
          if (horizon == null || lastTs.compareTo(horizon!) < 0) horizon = lastTs;
        }
      }
    }

    await scan(0);
    if (!_canRun) return const _Fetched([], false);
    await scan(1);
    if (!_canRun) return const _Fetched([], false);

    entries.sort((a, b) {
      var c = a.ts.compareTo(b.ts);
      if (c != 0) return c;
      c = a.src.compareTo(b.src);
      return c != 0 ? c : a.order.compareTo(b.order);
    });
    final floor = cur == null ? null : '${cur.ts}#${cur.count.toString().padLeft(6, '0')}';
    final out = <_Entry>[];
    String? groupTs;
    var n = 0;
    for (final e in entries) {
      if (horizon != null && e.ts.compareTo(horizon!) >= 0) break;
      if (e.ts == groupTs) {
        n++;
      } else {
        groupTs = e.ts;
        n = 1;
      }
      e.cursor = '${e.ts}#${n.toString().padLeft(6, '0')}';
      if (floor != null && e.cursor.compareTo(floor) <= 0) continue;
      out.add(e);
    }
    return _Fetched(out, more);
  }

  // ---- feedback ------------------------------------------------------------------------

  Future<void> _loadFeedback() async {
    List<OutboxItem> items;
    try {
      items = await outbox.pending();
    } catch (_) {
      return;
    }
    _fbQueue
      ..clear()
      ..addAll([
        for (final i in items)
          if (!_fbDone.contains(i.id) &&
              !_fbBlocked.contains(i.id) &&
              (i.report['pc'] == null || i.report['pc'] == pcId))
            i,
      ]);
  }

  Future<_Feedback?> _nextFeedback() async {
    while (_fbQueue.isNotEmpty) {
      final item = _fbQueue.removeAt(0);
      try {
        Uint8List png = Uint8List(0);
        final file = item.png;
        if (file != null) {
          try {
            final bytes = await file.readAsBytes();
            if (bytes.isNotEmpty && bytes.length <= _maxFeedbackPngBytes) png = bytes;
          } catch (_) {}
        }
        if (!_canRun) return null;
        final chunks = <String>[];
        for (var at = 0; at < png.length; at += _maxFeedbackChunkBytes) {
          final end = at + _maxFeedbackChunkBytes > png.length ? png.length : at + _maxFeedbackChunkBytes;
          chunks.add(base64Encode(Uint8List.sublistView(png, at, end)));
        }
        final report = {...item.report, 'screenshot': null};
        return _Feedback(item.id, {
          'type': 'feedback-begin',
          'v': 1,
          'id': item.id,
          'report': report,
          'pngBytes': png.length,
          'chunks': chunks.length,
        }, chunks);
      } catch (_) {
        _fbBlocked.add(item.id);
      }
    }
    return null;
  }

  // ---- stopping ------------------------------------------------------------------------

  void _stop(String reason) {
    if (_stopped) return;
    _stopped = true;
    _stopReason = reason;
    _cancelAll();
  }

  void _cancelAll() {
    for (final t in [_cooldown, _wake, _ackTimer, _retryTimer, _fbTimer, _fbRetryTimer]) {
      t?.cancel();
    }
    _cooldown = _wake = _ackTimer = _retryTimer = _fbTimer = _fbRetryTimer = null;
  }
}

int _clampInt(Object? v, int fallback, int min, int max) {
  final n = v is num ? v.toInt() : fallback;
  return n < min ? min : (n > max ? max : n);
}

Map<String, Object?>? _decode(String raw) {
  try {
    final j = jsonDecode(raw);
    return j is Map ? j.cast<String, Object?>() : null;
  } catch (_) {
    return null;
  }
}

class _Cur {
  const _Cur(this.ts, this.count);

  final String ts;
  final int count;

  static _Cur? parse(String? text) {
    if (text == null) return null;
    final i = text.lastIndexOf('#');
    if (i < 10) return null;
    final count = int.tryParse(text.substring(i + 1));
    return count == null ? null : _Cur(text.substring(0, i), count);
  }
}

class _Entry {
  _Entry(this.raw, this.ts, this.src, this.order);

  final String raw;
  final String ts;
  final int src;
  final int order;
  String cursor = '';
}

class _Fetched {
  const _Fetched(this.entries, this.more);

  final List<_Entry> entries;
  final bool more;
}

class _Batch {
  _Batch(this.id, this.to, this.msg);

  final int id;
  final String to;
  final Map<String, Object?> msg;
  bool sent = false;
  bool waitingRetry = false;
}

class _Step {
  const _Step(this.msg);

  final Map<String, Object?> msg;
}

class _Feedback {
  _Feedback(this.id, this.begin, this.chunks);

  final String id;
  final Map<String, Object?> begin;
  final List<String> chunks;

  /// 0: begin is next; i + 1: chunk i is next; chunks.length + 1: all sent.
  int _at = 0;

  bool get awaitingAck => _at > chunks.length;

  _Step next() {
    if (_at == 0) return _Step(begin);
    final i = _at - 1;
    return _Step({'type': 'feedback-chunk', 'v': 1, 'id': id, 'i': i, 'data': chunks[i]});
  }

  void advance() => _at++;
}
