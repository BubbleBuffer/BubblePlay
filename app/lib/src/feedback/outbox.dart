// The feedback outbox: reports waiting for a PC. Each report is one JSON file plus an optional
// PNG with the same base name in `<logs root>/outbox/`:
//
//   <yyyyMMdd'T'HHmmssSSS'Z'>-<report id>.json
//   <yyyyMMdd'T'HHmmssSSS'Z'>-<report id>.png
//
// Phase B (the session upload) reads [Outbox.pending], sends each item to the PC it connects
// to, and calls [Outbox.markSent] once the PC has acknowledged it. Nothing else deletes an
// outbox file. The PNG is written first and the JSON last, so a JSON file is always complete.

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import '../logging/log_store.dart';
import 'report.dart';

/// One queued report.
class OutboxItem {
  const OutboxItem({required this.id, required this.report, required this.json, this.png});

  /// The report's UUID: the argument of [Outbox.markSent].
  final String id;

  /// The report as stored (`screenshot` names [png]'s file, or is null).
  final Map<String, Object?> report;
  final File json;

  /// The screenshot, when the report has one and the file is still there.
  final File? png;
}

class Outbox {
  Outbox(this.dir);

  /// The outbox for a log root (`<root>/outbox`).
  factory Outbox.forRoot(Directory logsRoot) => Outbox(Directory('${logsRoot.path}/outbox'));

  final Directory dir;

  static final RegExp _name = RegExp(r'^(\d{8}T\d{9}Z)-([A-Za-z0-9_-]{1,64})\.json$');

  /// Stores [report] (and [screenshot], when given) and returns the item. The report's
  /// `screenshot` entry must already name the PNG, [screenshotName] gives that name.
  Future<OutboxItem> add(FeedbackReport report, {Uint8List? screenshot}) async {
    await dir.create(recursive: true);
    final base = '${_stamp(report.created)}-${report.id}';
    File? png;
    if (screenshot != null) {
      png = File('${dir.path}/$base.png');
      await _writeAtomic(png, screenshot);
    }
    final json = File('${dir.path}/$base.json');
    await _writeAtomic(json, utf8.encode(jsonEncode(report.json)));
    return OutboxItem(id: report.id, report: report.json, json: json, png: png);
  }

  /// The file name the PNG of [report] gets (what `screenshot` in the JSON must say).
  static String screenshotName(String id, DateTime created) => '${_stamp(created)}-$id.png';

  /// The reports waiting to be sent, oldest first. Unreadable files are skipped.
  Future<List<OutboxItem>> pending() async {
    if (!dir.existsSync()) return const [];
    final out = <OutboxItem>[];
    await for (final e in dir.list(followLinks: false)) {
      if (e is! File) continue;
      final name = e.uri.pathSegments.last;
      final m = _name.firstMatch(name);
      if (m == null) continue;
      try {
        final decoded = jsonDecode(await e.readAsString(encoding: utf8));
        if (decoded is! Map) continue;
        final png = File('${dir.path}/${m[1]}-${m[2]}.png');
        out.add(OutboxItem(
          id: m[2]!,
          report: decoded.cast<String, Object?>(),
          json: e,
          png: png.existsSync() ? png : null,
        ));
      } catch (_) {}
    }
    out.sort((a, b) => a.json.path.compareTo(b.json.path));
    return out;
  }

  /// Removes the report [id] (and its PNG) once the PC has taken it. Unknown ids do nothing.
  Future<void> markSent(String id) async {
    if (!isSafeId(id) || !dir.existsSync()) return;
    await for (final e in dir.list(followLinks: false)) {
      if (e is! File) continue;
      final name = e.uri.pathSegments.last;
      if (name.endsWith('-$id.json') || name.endsWith('-$id.png')) {
        try {
          await e.delete();
        } catch (_) {}
      }
    }
  }

  Future<void> _writeAtomic(File file, List<int> bytes) async {
    final tmp = File('${file.path}.tmp');
    await tmp.writeAsBytes(bytes, flush: true);
    await tmp.rename(file.path);
  }
}

String _stamp(DateTime created) {
  final u = created.toUtc();
  String two(int n) => n.toString().padLeft(2, '0');
  final ms = u.millisecond.toString().padLeft(3, '0');
  return '${u.year.toString().padLeft(4, '0')}${two(u.month)}${two(u.day)}'
      'T${two(u.hour)}${two(u.minute)}${two(u.second)}${ms}Z';
}
