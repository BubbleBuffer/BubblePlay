import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';

import '../model/model.dart' as model;

/// What one `GET /presence` answered.
class PresenceReply {
  const PresenceReply(this.status, this.body, {this.retryAfter});

  final int status;
  final String body;

  /// The `Retry-After` header in seconds, when sent.
  final int? retryAfter;
}

/// Sends one GET and returns the answer; throws on any transport error.
typedef PresenceFetcher = Future<PresenceReply> Function(Uri url);

/// The relay allows this many ids per request.
const int presenceMaxIds = 20;

/// The real fetcher: dart:io [HttpClient], short timeouts, no cookies or credentials.
Future<PresenceReply> httpPresenceFetch(Uri url) async {
  final client = HttpClient()..connectionTimeout = const Duration(seconds: 8);
  try {
    final request = await client.getUrl(url).timeout(const Duration(seconds: 8));
    final response = await request.close().timeout(const Duration(seconds: 8));
    final body = await utf8.decodeStream(response).timeout(const Duration(seconds: 8));
    final retry = int.tryParse(response.headers.value('retry-after') ?? '');
    return PresenceReply(response.statusCode, body, retryAfter: retry);
  } finally {
    client.close(force: true);
  }
}

/// Asks each relay which of the saved PCs are online, and remembers the last answer per PC.
///
/// Any failure leaves the last value in place (a PC never answered for reads as not online) and
/// a 429 pauses that relay for its `Retry-After`. Ids are never logged.
class PresenceService {
  PresenceService({PresenceFetcher? fetcher, DateTime Function()? now})
      : _fetch = fetcher ?? httpPresenceFetch,
        _now = now ?? DateTime.now;

  final PresenceFetcher _fetch;
  final DateTime Function() _now;
  final Map<String, bool> _values = {};
  final Map<String, DateTime> _backoff = {};
  bool _polling = false;

  /// The last known online state per normalised PC id.
  Map<String, bool> get values => Map.unmodifiable(_values);

  /// The last answer for [id], or null when it was never answered.
  bool? online(String id) => _values[normalizeId(id)];

  /// When [origin] may be asked again after a 429, if it is backed off.
  DateTime? backedOffUntil(String origin) {
    final until = _backoff[origin];
    return until != null && until.isAfter(_now()) ? until : null;
  }

  /// Asks the relays about [pcs]; returns true when any known value changed.
  Future<bool> poll(List<model.RemotePc> pcs) async {
    if (_polling) return false;
    _polling = true;
    try {
      final byOrigin = <String, List<String>>{};
      for (final pc in pcs) {
        final origin = originOf(pc.relay);
        if (origin == null) continue;
        final list = byOrigin.putIfAbsent(origin, () => []);
        if (!list.contains(pc.id)) list.add(pc.id);
      }
      final wanted = {for (final pc in pcs) normalizeId(pc.id)};
      _values.removeWhere((id, _) => !wanted.contains(id));
      var changed = false;
      for (final entry in byOrigin.entries) {
        if (backedOffUntil(entry.key) != null) continue;
        final ids = entry.value;
        for (var i = 0; i < ids.length; i += presenceMaxIds) {
          final end = i + presenceMaxIds > ids.length ? ids.length : i + presenceMaxIds;
          final result = await _ask(entry.key, ids.sublist(i, end));
          if (result == null) break;
          for (final r in result.entries) {
            if (_values[r.key] != r.value) {
              _values[r.key] = r.value;
              changed = true;
            }
          }
        }
      }
      return changed;
    } finally {
      _polling = false;
    }
  }

  Future<Map<String, bool>?> _ask(String origin, List<String> ids) async {
    try {
      final url = Uri.parse(origin).replace(
        path: '/presence',
        queryParameters: {'ids': ids.join(',')},
      );
      final reply = await _fetch(url);
      if (reply.status == 429) {
        final secs = (reply.retryAfter ?? 30).clamp(1, 3600);
        _backoff[origin] = _now().add(Duration(seconds: secs));
        return null;
      }
      if (reply.status != 200) return null;
      final decoded = jsonDecode(reply.body);
      if (decoded is! Map) return null;
      final wanted = {for (final id in ids) normalizeId(id)};
      final out = <String, bool>{};
      decoded.forEach((key, value) {
        final id = normalizeId('$key');
        if (value is bool && wanted.contains(id)) out[id] = value;
      });
      return out;
    } catch (e) {
      if (kDebugMode) debugPrint('presence: request failed (${e.runtimeType})');
      return null;
    }
  }

  /// The origin (scheme and authority) of a PC's relay URL, or null when it has none usable.
  static String? originOf(String relay) {
    final uri = Uri.tryParse(relay.trim());
    if (uri == null || !uri.hasAuthority || uri.host.isEmpty) return null;
    if (uri.scheme != 'https' && uri.scheme != 'http') return null;
    return '${uri.scheme}://${uri.authority}';
  }

  /// Ids compare without dashes and in upper case.
  static String normalizeId(String id) => id.replaceAll('-', '').toUpperCase();
}
