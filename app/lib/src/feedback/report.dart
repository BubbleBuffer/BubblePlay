// The feedback report (logging spec section 4), as plain values with no Flutter or bridge
// imports, so unit tests run without either. Shape copied from Scratch's report.dart; the
// report is JSON instead of a vault page, queued in the outbox for the PC.

import 'dart:math';

import '../logging/app_log.dart';

/// What kind of note this is. The names are what the JSON carries.
enum FeedbackKind {
  bug('Bug'),
  complaint('Complaint'),
  idea('Idea'),
  other('Other');

  const FeedbackKind(this.label);

  /// The name shown to people.
  final String label;
}

class FeedbackApp {
  const FeedbackApp({required this.version, required this.build, required this.commit});

  final String version;
  final String build;

  /// Empty when the build did not set `GIT_COMMIT`.
  final String commit;
}

class FeedbackDevice {
  const FeedbackDevice({required this.id, required this.name, required this.model});

  final String id;
  final String name;
  final String model;
}

/// A random version 4 UUID.
String newUuid([Random? random]) {
  final r = random ?? Random.secure();
  final bytes = List<int>.generate(16, (_) => r.nextInt(256));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  String hex(int from, int to) =>
      bytes.sublist(from, to).map((b) => b.toRadixString(16).padLeft(2, '0')).join();
  return '${hex(0, 4)}-${hex(4, 6)}-${hex(6, 8)}-${hex(8, 10)}-${hex(10, 16)}';
}

/// One report, ready to store. [json] has every string already redacted.
class FeedbackReport {
  const FeedbackReport({required this.id, required this.created, required this.json});

  final String id;

  /// UTC time of creation.
  final DateTime created;

  /// The report as stored. `screenshot` names the PNG next to it, or is null.
  final Map<String, Object?> json;
}

/// Builds the report. [text] and every string in the result pass through the logger's
/// redaction (registered secrets included). [logs] and [errors] come from
/// [AppLogger.reportLogs]. [screenshotFile] is the PNG's file name when one is stored with it.
FeedbackReport buildReport({
  required AppLogger log,
  required DateTime now,
  required String text,
  required FeedbackKind kind,
  required FeedbackApp app,
  required String platform,
  required FeedbackDevice device,
  required String? pc,
  required String? session,
  required String route,
  required String locale,
  required ReportLogs logs,
  String? screenshotFile,
  String? id,
}) {
  final reportId = id ?? newUuid();
  final utc = now.toUtc();
  final json = <String, Object?>{
    'version': 1,
    'id': reportId,
    'created': _ts(utc),
    'text': text,
    'kind': kind.name,
    'app': {'version': app.version, 'build': app.build, 'commit': app.commit},
    'platform': platform,
    'device': {'id': device.id, 'name': device.name, 'model': device.model},
    'pc': pc,
    'session': session,
    'route': route,
    'locale': locale,
    'logs': logs.logs.length > ringLines
        ? logs.logs.sublist(logs.logs.length - ringLines)
        : logs.logs,
    'errors': logs.errors.length > ringErrors
        ? logs.errors.sublist(logs.errors.length - ringErrors)
        : logs.errors,
    'screenshot': screenshotFile,
  };
  final clean = log.redactJson(json)! as Map<String, Object?>;
  // The id and time are ours; redaction must not turn them into something else.
  clean['id'] = reportId;
  clean['created'] = _ts(utc);
  // The file name is a UUID plus a stamp, longer than the blob rule allows.
  clean['screenshot'] = screenshotFile;
  return FeedbackReport(id: reportId, created: utc, json: clean);
}

/// UTC RFC3339 with three fractional digits and Z.
String _ts(DateTime utc) {
  final s = utc.toIso8601String();
  final dot = s.indexOf('.');
  return '${s.substring(0, dot + 4)}Z';
}
