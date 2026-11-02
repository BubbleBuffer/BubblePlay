// Redaction for log lines and feedback reports (logging spec section 2). Ported from Scratch's
// report.dart redact() and extended: sensitive field names, URL parameters, Bearer and
// Authorization, key=value in free text, registered literal secrets and long opaque blobs.
//
// Never log a secret in the first place; this is the net. Nothing here throws.

/// The text that replaces anything secret.
const redacted = '[redacted]';

/// The text that replaces a long opaque run (a key, proof, ticket or base64 blob).
const redactedBlob = '[redacted-blob]';

/// Words that make a field name sensitive. Matched per word of the name (split on `_ - .`,
/// spaces and camelCase). [_exactWords] must be the whole word (or its plural); the rest match
/// anywhere inside a word.
const _containsWords = [
  'password',
  'passphrase',
  'passwd',
  'secret',
  'token',
  'proof',
  'ticket',
  'pairing',
  'signature',
  'cookie',
  'credential',
];
const _exactWords = ['key', 'code', 'psk'];

/// Words that look like a sensitive one but are not (`keyboard`, `keyframe`, `keycode`, `keyid`).
const _allowedWords = {'keyboard', 'keyframe', 'keycode', 'keyid'};

final _wordSplit = RegExp(r'[_\-.\s]+');
final _camelSplit = RegExp(r'(?<=[a-z0-9])(?=[A-Z])');

/// Whether a field called [name] holds something that must never be written.
bool isSensitiveKey(String name) {
  try {
    final words = <String>[];
    for (final part in name.split(_wordSplit)) {
      for (final word in part.split(_camelSplit)) {
        if (word.isNotEmpty) words.add(word.toLowerCase());
      }
    }
    // `device_key_id`, `keyId` and the like name an id, not a key.
    for (var i = 0; i + 1 < words.length; i++) {
      if (words[i] == 'key' && words[i + 1] == 'id') {
        words[i] = 'keyid';
        words[i + 1] = '';
      }
    }
    for (final word in words) {
      if (word.isEmpty || _allowedWords.contains(word)) continue;
      for (final w in _exactWords) {
        if (word == w || word == '${w}s') return true;
      }
      if (word.startsWith('auth')) return true;
      for (final w in _containsWords) {
        if (word.contains(w)) return true;
      }
    }
    return false;
  } catch (_) {
    return true;
  }
}

/// Parameter names whose value is dropped from a URL query or fragment.
const _urlParams = 'token|key|secret|password|passphrase|code|ticket|proof|pairing|sig';

final _urlParam = RegExp(
  '([?&#;](?:$_urlParams)=)[^&\\s#"\'<>]*',
  caseSensitive: false,
);
final _bearer = RegExp(r'Bearer\s+[^\s"<>]+', caseSensitive: false);
final _authorizationLine = RegExp(r'authorization\s*:[^\r\n]*', caseSensitive: false);

/// `"name": "value"` and `"name": 123` (JSON text inside a message).
final _jsonPair = RegExp(r'"([A-Za-z0-9_.\-]+)"(\s*:\s*)("(?:[^"\\]|\\.)*"|[^\s,}\]]+)');

/// The start of `name=value` or `name: value` (the key and the separator), and the value that
/// follows: quoted, or one bare word. Scanned by hand in [_redactPairs] so a value that holds
/// another pair (`failed: token=abc`) is still looked into.
final _keySep = RegExp(r'(?<![A-Za-z0-9_.\-])([A-Za-z][A-Za-z0-9_.\-]*)\s*[=:]\s*');
final _pairValue = RegExp(r'''\[redacted(?:-blob)?\]|"[^"]*"|'[^']*'|[^\s,;&}\])"']+''');

String _redactPairs(String text) {
  final out = StringBuffer();
  var pos = 0;
  for (final m in _keySep.allMatches(text)) {
    if (m.start < pos || !isSensitiveKey(m[1]!)) continue;
    final value = _pairValue.matchAsPrefix(text, m.end);
    if (value == null || value[0]!.contains(redacted)) continue;
    out
      ..write(text.substring(pos, m.end))
      ..write(redacted);
    pos = value.end;
  }
  out.write(text.substring(pos));
  return out.toString();
}

final _blob = RegExp(r'[A-Za-z0-9+/_=\-]{40,}');
final _blobNoSlash = RegExp(r'[A-Za-z0-9+_=\-]{40,}');

/// Redacts the free text [text]: rules 2 to 6 of the spec. [secrets] are the registered
/// literals. [stack] relaxes rule 6 to runs without `/`, so long package paths in a stack trace
/// are kept while a key or base64 blob is not.
String redactText(String text, {Iterable<String> secrets = const [], bool stack = false}) {
  var out = text;
  try {
    out = out.replaceAllMapped(_authorizationLine, (_) => 'Authorization: $redacted');
    out = out.replaceAll(_bearer, 'Bearer $redacted');
    out = out.replaceAllMapped(_urlParam, (m) => '${m[1]}$redacted');
    out = out.replaceAllMapped(_jsonPair, (m) {
      final value = m[3]!;
      if (!isSensitiveKey(m[1]!) || value == '"$redacted"' || value == redacted) return m[0]!;
      return '"${m[1]}"${m[2]}"$redacted"';
    });
    out = _redactPairs(out);
    // Longest first, so a secret that contains a shorter one is removed whole.
    final literal = [
      for (final s in secrets)
        if (s.trim().length >= minSecretLength) s.trim(),
    ]..sort((a, b) => b.length.compareTo(a.length));
    for (final s in literal) {
      out = out.replaceAll(s, redacted);
    }
    out = out.replaceAll(stack ? _blobNoSlash : _blob, redactedBlob);
  } catch (_) {
    return '[redaction failed]';
  }
  return out;
}

/// Registered secrets shorter than this are ignored: they would blot out ordinary text.
const minSecretLength = 4;

/// Redacts a field value that sits under [key]: the whole value goes when the name is
/// sensitive, otherwise every string inside it is redacted as free text. Maps and lists are
/// walked; other values pass through.
Object? redactField(String key, Object? value, {Iterable<String> secrets = const []}) {
  try {
    if (isSensitiveKey(key)) return value == null ? null : redacted;
    return _walk(value, secrets, key == 'stack');
  } catch (_) {
    return redacted;
  }
}

Object? _walk(Object? value, Iterable<String> secrets, bool stack) {
  if (value is String) return redactText(value, secrets: secrets, stack: stack);
  if (value is Map) {
    return {
      for (final e in value.entries)
        '${e.key}': redactField('${e.key}', e.value, secrets: secrets),
    };
  }
  if (value is Iterable) return [for (final v in value) _walk(v, secrets, stack)];
  return value;
}
