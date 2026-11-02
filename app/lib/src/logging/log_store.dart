// The on-device log store (logging spec section 3):
//
//   <root>/pc/<pc-id>/<src>-YYYY-MM-DD.jsonl   lines that relate to a PC
//   <root>/misc/<src>-YYYY-MM-DD.jsonl         lines that do not
//
// The Dart process writes `app-win` / `app-android` files; the Kotlin core writes its own
// `app-android-core` files in the same tree. Nobody appends to another process's file, so no
// file is shared for writing. Reading merges every file by `ts`. Caps and retention are
// enforced here for the whole tree, but only this process's own files are ever trimmed.

import 'dart:convert';
import 'dart:io';

/// A PC or device id that is safe to use as a directory name: `[A-Za-z0-9_-]`, 1 to 64 chars.
final RegExp _safeId = RegExp(r'^[A-Za-z0-9_-]{1,64}$');

/// Whether [id] may name a directory in the store.
bool isSafeId(String? id) => id != null && _safeId.hasMatch(id);

final RegExp _dayFile = RegExp(r'^(.+)-(\d{4}-\d{2}-\d{2})\.jsonl$');

/// `YYYY-MM-DD` of a UTC time.
String dayOf(DateTime utc) =>
    '${utc.year.toString().padLeft(4, '0')}-${utc.month.toString().padLeft(2, '0')}-'
    '${utc.day.toString().padLeft(2, '0')}';

/// One line read back from the store.
class StoredLine {
  const StoredLine(this.raw, this.ts, this.level, this.order);

  /// The JSONL text, without the newline.
  final String raw;

  /// The line's `ts` (UTC RFC3339 with milliseconds, so it sorts as text).
  final String ts;
  final String level;

  /// Position in the file walk, to keep the sort stable.
  final int order;

  Map<String, Object?> get json => (jsonDecode(raw) as Map).cast<String, Object?>();
}

/// A page of lines for the upload (phase B) and the reports.
class LogPage {
  const LogPage(this.lines, this.cursor, {this.more = false});

  /// Raw JSONL lines, oldest first.
  final List<String> lines;

  /// Pass this as `afterCursor` to get the lines after the last one here. When [lines] is empty
  /// it is the cursor that was passed in (null for "from the start").
  final String? cursor;

  /// More lines are waiting beyond this page.
  final bool more;
}

class LogStore {
  LogStore(
    this.root, {
    required this.src,
    DateTime Function()? clock,
    this.perPcCap = 5 * 1024 * 1024,
    this.totalCap = 20 * 1024 * 1024,
    this.retention = const Duration(days: 30),
  }) : _clock = clock ?? DateTime.now;

  final Directory root;

  /// This process's source name, the prefix of its files.
  final String src;
  final DateTime Function() _clock;
  final int perPcCap;
  final int totalCap;
  final Duration retention;

  int _sinceEnforce = 0;

  Directory _dirFor(String? pc) =>
      pc == null ? Directory('${root.path}/misc') : Directory('${root.path}/pc/$pc');

  File _fileFor(String? pc, String day) => File('${_dirFor(pc).path}/$src-$day.jsonl');

  /// Appends [lines] (each one JSON object, no newline) to [pc]'s file for each line's day.
  /// [pc] must already be a safe id, or null for the misc file.
  Future<void> append(String? pc, Map<String, List<String>> linesByDay) async {
    for (final entry in linesByDay.entries) {
      if (entry.value.isEmpty) continue;
      final file = _fileFor(pc, entry.key);
      await file.parent.create(recursive: true);
      final text = '${entry.value.join('\n')}\n';
      await file.writeAsString(text, mode: FileMode.append, encoding: utf8);
      _sinceEnforce += text.length;
    }
  }

  /// True once enough has been written since the last [enforceCaps] to be worth checking.
  bool get wantsEnforce => _sinceEnforce > 256 * 1024;

  /// Applies retention, the per-PC cap and the total cap. Never throws.
  Future<void> enforceCaps() async {
    _sinceEnforce = 0;
    try {
      final today = dayOf(_clock().toUtc());
      final oldest = dayOf(_clock().toUtc().subtract(retention));
      final files = await _scan();
      final alive = <_StoreFile>[];
      for (final f in files) {
        if (f.day.compareTo(oldest) < 0) {
          await _delete(f.file);
        } else {
          alive.add(f);
        }
      }
      final byPc = <String, List<_StoreFile>>{};
      for (final f in alive) {
        if (f.pc != null) byPc.putIfAbsent(f.pc!, () => []).add(f);
      }
      for (final group in byPc.values) {
        await _shrink(group, perPcCap, today);
      }
      final remaining = <_StoreFile>[
        for (final f in alive)
          if (f.file.existsSync()) f,
      ];
      await _shrink(remaining, totalCap, today);
      await _removeEmptyPcDirs();
    } catch (_) {}
  }

  Future<List<_StoreFile>> _scan() async {
    final out = <_StoreFile>[];
    Future<void> add(Directory dir, String? pc) async {
      if (!dir.existsSync()) return;
      await for (final e in dir.list(followLinks: false)) {
        if (e is! File) continue;
        final name = e.uri.pathSegments.last;
        final m = _dayFile.firstMatch(name);
        if (m == null) continue;
        try {
          out.add(_StoreFile(e, pc, m[1]!, m[2]!, await e.length()));
        } catch (_) {}
      }
    }

    await add(Directory('${root.path}/misc'), null);
    final pcRoot = Directory('${root.path}/pc');
    if (pcRoot.existsSync()) {
      await for (final e in pcRoot.list(followLinks: false)) {
        if (e is! Directory) continue;
        final id = e.uri.pathSegments.where((s) => s.isNotEmpty).last;
        if (isSafeId(id)) await add(e, id);
      }
    }
    return out;
  }

  /// Brings [files] under [cap]: older day files go first; if only today's files are left and
  /// they are still over, this process's own file for today keeps its newest half.
  Future<void> _shrink(List<_StoreFile> files, int cap, String today) async {
    var total = files.fold<int>(0, (sum, f) => sum + f.size);
    if (total <= cap) return;
    final old = [
      for (final f in files)
        if (f.day.compareTo(today) < 0) f,
    ]..sort((a, b) {
        final byDay = a.day.compareTo(b.day);
        return byDay != 0 ? byDay : a.file.path.compareTo(b.file.path);
      });
    for (final f in old) {
      if (total <= cap) return;
      if (await _delete(f.file)) total -= f.size;
    }
    if (total <= cap) return;
    final own = [
      for (final f in files)
        if (f.day == today && f.src == src && f.file.existsSync()) f,
    ]..sort((a, b) => b.size.compareTo(a.size));
    for (final f in own) {
      if (total <= cap) return;
      final over = total - cap;
      // Leave some headroom so the next check does not trim again at once.
      final keep = (f.size - over - cap ~/ 10).clamp(0, f.size);
      final freed = await _keepTail(f.file, keep);
      total -= freed;
    }
  }

  /// Rewrites [file] with its last [keepBytes] bytes, from a line start. Returns bytes freed.
  Future<int> _keepTail(File file, int keepBytes) async {
    try {
      final bytes = await file.readAsBytes();
      if (bytes.length <= keepBytes) return 0;
      var start = bytes.length - keepBytes;
      while (start < bytes.length && bytes[start - 1] != 0x0A) {
        start++;
      }
      await file.writeAsBytes(bytes.sublist(start));
      return start;
    } catch (_) {
      return 0;
    }
  }

  Future<bool> _delete(File file) async {
    try {
      await file.delete();
      return true;
    } catch (_) {
      return false;
    }
  }

  Future<void> _removeEmptyPcDirs() async {
    final pcRoot = Directory('${root.path}/pc');
    if (!pcRoot.existsSync()) return;
    await for (final e in pcRoot.list(followLinks: false)) {
      if (e is Directory && e.listSync().isEmpty) {
        try {
          await e.delete();
        } catch (_) {}
      }
    }
  }

  /// All lines of [pc]'s files (or the misc files when [pc] is null), every source merged and
  /// sorted by `ts`, oldest first. Lines that are not JSON with a `ts` are skipped.
  Future<List<StoredLine>> readLines(String? pc, {String? fromDay}) async {
    final dir = _dirFor(pc);
    if (!dir.existsSync()) return const [];
    final names = <File>[];
    await for (final e in dir.list(followLinks: false)) {
      if (e is! File) continue;
      final m = _dayFile.firstMatch(e.uri.pathSegments.last);
      if (m == null) continue;
      if (fromDay != null && m[2]!.compareTo(fromDay) < 0) continue;
      names.add(e);
    }
    names.sort((a, b) => a.path.compareTo(b.path));
    final out = <StoredLine>[];
    var order = 0;
    for (final file in names) {
      List<String> lines;
      try {
        lines = const LineSplitter().convert(await file.readAsString(encoding: utf8));
      } catch (_) {
        try {
          lines = const LineSplitter()
              .convert(utf8.decode(await file.readAsBytes(), allowMalformed: true));
        } catch (_) {
          continue;
        }
      }
      for (final line in lines) {
        if (line.isEmpty) continue;
        try {
          final json = jsonDecode(line);
          if (json is! Map) continue;
          final ts = json['ts'];
          if (ts is! String) continue;
          out.add(StoredLine(line, ts, '${json['level']}', order++));
        } catch (_) {}
      }
    }
    out.sort((a, b) {
      final c = a.ts.compareTo(b.ts);
      return c != 0 ? c : a.order.compareTo(b.order);
    });
    return out;
  }

  /// The lines of [pc] (or misc) after [afterCursor], at most [limit] of them.
  Future<LogPage> page(String? pc, {String? afterCursor, int limit = 500}) async {
    final cursor = _Cursor.parse(afterCursor);
    final lines = await readLines(pc, fromDay: cursor?.ts.substring(0, 10));
    final out = <String>[];
    var seen = 0;
    String? lastTs;
    String? outTs;
    var lastCount = 0;
    var more = false;
    for (final line in lines) {
      if (line.ts == lastTs) {
        seen++;
      } else {
        lastTs = line.ts;
        seen = 1;
      }
      if (cursor != null) {
        final c = line.ts.compareTo(cursor.ts);
        if (c < 0 || (c == 0 && seen <= cursor.count)) continue;
      }
      if (out.length >= limit) {
        more = true;
        break;
      }
      out.add(line.raw);
      outTs = line.ts;
      lastCount = seen;
    }
    if (out.isEmpty) return LogPage(const [], afterCursor);
    return LogPage(out, _Cursor(outTs!, lastCount).encode(), more: more);
  }
}

class _Cursor {
  const _Cursor(this.ts, this.count);

  final String ts;

  /// How many lines with exactly [ts] were already delivered.
  final int count;

  static _Cursor? parse(String? text) {
    if (text == null) return null;
    final i = text.lastIndexOf('#');
    if (i < 10) return null;
    final count = int.tryParse(text.substring(i + 1));
    if (count == null) return null;
    return _Cursor(text.substring(0, i), count);
  }

  String encode() => '$ts#$count';
}

class _StoreFile {
  _StoreFile(this.file, this.pc, this.src, this.day, this.size);

  final File file;
  final String? pc;
  final String src;
  final String day;
  final int size;
}
