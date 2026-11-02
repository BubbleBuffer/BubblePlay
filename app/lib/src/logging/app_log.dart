// The app's logger (logging spec sections 1 to 3). Static facade [AppLog] over one [AppLogger].
//
// Rules it keeps: it never throws into a caller, never blocks (a line is redacted, encoded and
// queued; a timer writes the queue in batches), the queue is bounded (oldest dropped, the drop
// count logged once a minute), and nothing secret reaches the file (see redact.dart).
//
// Until [AppLog.install] gives it a store (tests, the first moments of start-up) it only keeps
// the in-memory ring, so a widget test never starts a timer.

import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:io';

import 'log_store.dart';
import 'redact.dart';

enum LogLevel {
  trace,
  debug,
  info,
  warn,
  error;

  /// The lowercase name written to the file.
  String get wire => name;
}

/// A line is at most this many bytes with its newline.
const maxLineBytes = 8 * 1024;

const _maxMsgChars = 2000;
const _maxStackChars = 4096;
const _maxFieldChars = 1024;
const _truncated = '...[truncated]';

/// Lines kept in memory for reports when the store has none (and for the last-N reads).
const ringLines = 200;

/// Error lines kept in memory.
const ringErrors = 20;

String _cut(String text, int max) =>
    text.length <= max ? text : '${text.substring(0, max - _truncated.length)}$_truncated';

/// The last lines and errors a report carries.
class ReportLogs {
  const ReportLogs(this.logs, this.errors);

  /// The last [ringLines] lines, oldest first, as JSON objects.
  final List<Map<String, Object?>> logs;

  /// The last [ringErrors] error-level lines, oldest first.
  final List<Map<String, Object?>> errors;
}

class AppLogger {
  AppLogger({
    required this.src,
    LogStore? store,
    this.device,
    this.level = LogLevel.info,
    DateTime Function()? clock,
    this.queueLimit = 2000,
    this.flushDelay = const Duration(milliseconds: 300),
  })
      : _store = store, // ignore: prefer_initializing_formals
        _clock = clock ?? DateTime.now;

  /// `app-win` or `app-android`.
  final String src;
  final LogStore? _store;
  final DateTime Function() _clock;
  final int queueLimit;
  final Duration flushDelay;

  /// Lines below this level are dropped.
  LogLevel level;

  /// This device's own id, once the core has told us.
  String? device;
  String? _pc;
  String? _session;

  final Set<String> _secrets = {};
  final Queue<_Pending> _queue = Queue<_Pending>();
  final Queue<String> _ring = Queue<String>();
  final Queue<String> _errorRing = Queue<String>();
  Timer? _timer;
  Future<void>? _draining;
  int _dropped = 0;
  DateTime? _lastDropNote;
  bool _writeFailed = false;

  /// Lines dropped because the queue was full, not yet reported.
  int get droppedPending => _dropped;

  LogStore? get store => _store;

  /// The PC and session that following lines relate to (null clears it).
  void setContext({String? pc, String? session}) {
    _pc = pc;
    _session = session;
  }

  String? get pc => _pc;

  /// Registers a literal that must never appear in the file.
  void registerSecret(String secret) {
    if (secret.trim().length >= minSecretLength) _secrets.add(secret.trim());
  }

  /// Redacts [text] with this logger's registered secrets (for reports).
  String redact(String text) => redactText(text, secrets: _secrets);

  /// Redacts every string in [json], using the field names to decide (for reports).
  Object? redactJson(Object? json) => redactField('', json, secrets: _secrets);

  void log(
    LogLevel at,
    String target,
    String msg, {
    Object? error,
    StackTrace? stack,
    Map<String, Object?>? fields,
    String? pc,
  }) {
    try {
      if (at.index < level.index) return;
      final now = _clock().toUtc();
      String? linePc = pc ?? _pc;
      if (!isSafeId(linePc)) linePc = null;
      final merged = <String, Object?>{...?fields};
      if (error != null) {
        merged['error'] = error.toString();
        merged['error_type'] = error.runtimeType.toString();
      }
      if (stack != null) merged['stack'] = stack.toString();
      final text = _encode(now, at, target, msg, linePc, merged);
      _ring.addLast(text);
      while (_ring.length > ringLines) {
        _ring.removeFirst();
      }
      if (at == LogLevel.error) {
        _errorRing.addLast(text);
        while (_errorRing.length > ringErrors) {
          _errorRing.removeFirst();
        }
      }
      if (_store == null) return;
      _queue.addLast(_Pending(linePc, dayOf(now), text));
      while (_queue.length > queueLimit) {
        _queue.removeFirst();
        _dropped++;
      }
      _schedule(at == LogLevel.error ? Duration.zero : flushDelay);
    } catch (_) {
      // Logging never throws into the caller.
    }
  }

  String _ts(DateTime utc) {
    final s = utc.toIso8601String();
    // toIso8601String gives microseconds on some platforms; the spec wants exactly three.
    final dot = s.indexOf('.');
    return '${s.substring(0, dot + 4)}Z';
  }

  String _encode(
    DateTime now,
    LogLevel at,
    String target,
    String msg,
    String? pc,
    Map<String, Object?> fields,
  ) {
    Map<String, Object?> build(int msgMax, bool withFields) {
      Map<String, Object?> out;
      try {
        out = {
          'ts': _ts(now),
          'level': at.wire,
          'src': src,
          'device': device,
          'pc': pc,
          'session': _session,
          'target': target,
          'msg': _cut(redactText(msg, secrets: _secrets), msgMax),
          'fields': withFields ? _cleanFields(fields) : <String, Object?>{},
        };
      } catch (_) {
        out = {
          'ts': _ts(now),
          'level': at.wire,
          'src': src,
          'device': device,
          'pc': pc,
          'session': _session,
          'target': target,
          'msg': '[redaction failed]',
          'fields': <String, Object?>{},
        };
      }
      return out;
    }

    var map = build(_maxMsgChars, true);
    var text = jsonEncode(map);
    if (utf8.encode(text).length + 1 <= maxLineBytes) return text;
    map = build(500, false);
    map['fields'] = {'truncated': true};
    text = jsonEncode(map);
    var max = 500;
    while (utf8.encode(text).length + 1 > maxLineBytes && max > 10) {
      max ~/= 2;
      map = build(max, false);
      text = jsonEncode(map);
    }
    return text;
  }

  Map<String, Object?> _cleanFields(Map<String, Object?> fields) {
    final out = <String, Object?>{};
    for (final e in fields.entries) {
      var v = redactField(e.key, e.value, secrets: _secrets);
      if (v is String) v = _cut(v, e.key == 'stack' ? _maxStackChars : _maxFieldChars);
      out[e.key] = v;
    }
    return out;
  }

  void _schedule(Duration delay) {
    if (_store == null) return;
    final t = _timer;
    if (t != null) {
      if (delay != Duration.zero) return;
      t.cancel();
    }
    _timer = Timer(delay, () {
      _timer = null;
      unawaited(_drain());
    });
  }

  /// Writes everything queued so far. Never throws.
  Future<void> flush() async {
    _timer?.cancel();
    _timer = null;
    await _drain();
  }

  Future<void> _drain() async {
    // One drain at a time; wait for a running one, then drain what it left.
    while (_draining != null) {
      await _draining;
    }
    if (_queue.isEmpty && _dropped == 0) return;
    final future = _drainNow();
    _draining = future;
    try {
      await future;
    } finally {
      _draining = null;
    }
  }

  Future<void> _drainNow() async {
    final store = _store;
    if (store == null) return;
    try {
      _noteDropped();
      while (_queue.isNotEmpty) {
        final batch = _queue.toList();
        _queue.clear();
        final groups = <String?, Map<String, List<String>>>{};
        for (final p in batch) {
          groups.putIfAbsent(p.pc, () => {}).putIfAbsent(p.day, () => []).add(p.text);
        }
        for (final g in groups.entries) {
          try {
            await store.append(g.key, g.value);
            _writeFailed = false;
          } catch (_) {
            // Disk full or the directory is gone: drop the batch, never retry forever.
            _writeFailed = true;
          }
        }
      }
      if (store.wantsEnforce) await store.enforceCaps();
    } catch (_) {}
  }

  void _noteDropped() {
    if (_dropped == 0) return;
    final now = _clock();
    final last = _lastDropNote;
    if (last != null && now.difference(last) < const Duration(minutes: 1)) return;
    final n = _dropped;
    _dropped = 0;
    _lastDropNote = now;
    log(LogLevel.warn, 'AppLog', 'Log lines were dropped because the queue was full',
        fields: {'dropped': n});
  }

  /// True after a write failed (until the next one succeeds). For tests and diagnostics.
  bool get writeFailed => _writeFailed;

  /// Applies caps and retention now (start-up).
  Future<void> enforceCaps() async => _store?.enforceCaps();

  /// The last [ringLines] lines and [ringErrors] errors for a report: this PC's files and the
  /// misc files, every process merged by `ts`. Without a store, this process's ring.
  Future<ReportLogs> reportLogs({String? pc}) async {
    await flush();
    final store = _store;
    Map<String, Object?> parse(String line) => (jsonDecode(line) as Map).cast<String, Object?>();
    if (store == null) {
      return ReportLogs(
        [for (final l in _ring) parse(l)],
        [for (final l in _errorRing) parse(l)],
      );
    }
    final lines = <StoredLine>[
      ...await store.readLines(null),
      if (isSafeId(pc)) ...await store.readLines(pc),
    ]..sort((a, b) => a.ts.compareTo(b.ts));
    final errors = [
      for (final l in lines)
        if (l.level == 'error') l,
    ];
    return ReportLogs(
      [for (final l in lines.skip(lines.length > ringLines ? lines.length - ringLines : 0)) l.json],
      [
        for (final l in errors.skip(errors.length > ringErrors ? errors.length - ringErrors : 0))
          l.json,
      ],
    );
  }

  /// This process's in-memory ring of the last error lines, oldest first.
  List<String> get recentErrors => List.unmodifiable(_errorRing);

  /// This process's in-memory ring of the last lines, oldest first.
  List<String> get recentLines => List.unmodifiable(_ring);
}

class _Pending {
  _Pending(this.pc, this.day, this.text);

  final String? pc;
  final String day;
  final String text;
}

/// The app-wide logger. Call [install] once at start-up; every other call is safe before that.
class AppLog {
  AppLog._();

  static AppLogger _instance = AppLogger(src: defaultSrc);

  /// `app-android` on Android, `app-win` elsewhere.
  static String get defaultSrc => Platform.isAndroid ? 'app-android' : 'app-win';

  static AppLogger get instance => _instance;

  /// Replaces the logger (start-up, tests).
  static void install(AppLogger logger) => _instance = logger;

  /// Opens the real logger: files under [root] (see [resolveLogRoot]), caps applied now.
  static Future<AppLogger> open({
    required Directory root,
    String? src,
    LogLevel level = LogLevel.info,
  }) async {
    final name = src ?? defaultSrc;
    final logger = AppLogger(src: name, store: LogStore(root, src: name), level: level);
    install(logger);
    await logger.enforceCaps();
    return logger;
  }

  static void trace(String target, String msg, {Map<String, Object?>? fields, String? pc}) =>
      _instance.log(LogLevel.trace, target, msg, fields: fields, pc: pc);

  static void debug(String target, String msg, {Map<String, Object?>? fields, String? pc}) =>
      _instance.log(LogLevel.debug, target, msg, fields: fields, pc: pc);

  static void info(String target, String msg, {Map<String, Object?>? fields, String? pc}) =>
      _instance.log(LogLevel.info, target, msg, fields: fields, pc: pc);

  static void warn(
    String target,
    String msg, {
    Object? error,
    Map<String, Object?>? fields,
    String? pc,
  }) =>
      _instance.log(LogLevel.warn, target, msg, error: error, fields: fields, pc: pc);

  /// Logs an error line: [error] becomes `fields.error`, [stack] `fields.stack`.
  static void error(
    String target,
    String msg, {
    Object? error,
    StackTrace? stack,
    Map<String, Object?>? fields,
    String? pc,
  }) =>
      _instance.log(LogLevel.error, target, msg,
          error: error, stack: stack, fields: fields, pc: pc);

  /// Sets this device's own id (the `device` field of every line).
  static void setDevice(String? id) => _instance.device = id;

  /// Sets the PC and session that following lines relate to; null clears them.
  static void setContext({String? pc, String? session}) =>
      _instance.setContext(pc: pc, session: session);

  /// Registers a literal secret that must never reach the file.
  static void registerSecret(String secret) => _instance.registerSecret(secret);

  static Future<void> flush() => _instance.flush();

  /// For the upload (phase B): the stored lines that relate to [pcId], oldest first, every
  /// process's files merged by `ts`, after [afterCursor] (null: from the start).
  ///
  /// The returned [LogPage.cursor] is opaque (`<ts>#<n>`: the last line's `ts` and how many
  /// lines with exactly that `ts` were delivered). Pass it back to continue; nothing is
  /// delivered twice and nothing is skipped when several lines share a millisecond. Pending
  /// lines are written first. A [pcId] that is not a safe id returns nothing.
  static Future<LogPage> linesFor(String pcId, {String? afterCursor, int limit = 500}) async {
    final store = _instance.store;
    if (store == null || !isSafeId(pcId)) return LogPage(const [], afterCursor);
    await _instance.flush();
    return store.page(pcId, afterCursor: afterCursor, limit: limit);
  }

  /// Like [linesFor] for the lines that relate to no PC (uploaded to the next PC connected).
  static Future<LogPage> miscLines({String? afterCursor, int limit = 500}) async {
    final store = _instance.store;
    if (store == null) return LogPage(const [], afterCursor);
    await _instance.flush();
    return store.page(null, afterCursor: afterCursor, limit: limit);
  }
}

/// Where the log store lives (logging spec section 3): the app's own data directory plus
/// `logs`. Windows: `%LOCALAPPDATA%\BubblePlay\viewer\logs` (beside the Rust core's key and PC
/// list). Android: the app's files directory (the parent of the cache directory the Dart
/// runtime reports as its temp directory) plus `logs`. Elsewhere: the temp directory.
Directory resolveLogRoot({Map<String, String>? environment}) {
  final env = environment ?? Platform.environment;
  if (Platform.isWindows) {
    final local = env['LOCALAPPDATA'];
    if (local != null && local.isNotEmpty) {
      return Directory('$local\\BubblePlay\\viewer\\logs');
    }
  }
  if (Platform.isAndroid) {
    return Directory('${Directory.systemTemp.parent.path}/files/logs');
  }
  return Directory('${Directory.systemTemp.path}/bubbleplay-logs');
}
