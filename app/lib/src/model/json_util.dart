/// Readers and writers for the JSON contract shared with the Rust model in
/// `client-win/src/model.rs`.
///
/// Serde uses external tagging: a unit variant is a bare string
/// (`"Work"`, `"CancelFlow"`), every other variant is a single-key object
/// (`{"SetMode":"Game"}`, `{"PairEnter":{"remote":false,"error":null}}`).
/// The helpers here decode both shapes and throw [FormatException] on
/// anything else, so a broken payload fails loudly.
library;

/// A serde variant: a bare string (unit variant) or the single entry of a
/// single-key object (newtype, tuple or struct variant).
class JsonVariant {
  const JsonVariant(this.name, this.value);

  factory JsonVariant.parse(Object? json) {
    if (json is String) return JsonVariant(json, null);
    final map = jsonMap(json);
    if (map.length != 1) {
      throw FormatException(
        'Expected a single-key object, got ${map.length} keys',
      );
    }
    final entry = map.entries.single;
    return JsonVariant(entry.key, entry.value);
  }

  /// The Rust variant name.
  final String name;

  /// The payload: null for unit variants.
  final Object? value;
}

/// Reads [json] as a JSON object, copying maps with non-String keys.
Map<String, Object?> jsonMap(Object? json) {
  if (json is Map<String, Object?>) return json;
  if (json is Map) {
    final result = <String, Object?>{};
    for (final entry in json.entries) {
      final key = entry.key;
      if (key is! String) {
        throw FormatException('Expected string keys, got ${key.runtimeType}');
      }
      result[key] = entry.value;
    }
    return result;
  }
  throw FormatException('Expected a JSON object, got ${json.runtimeType}');
}

/// A required struct field; serde rejects an object that lacks one.
Object? jsonField(Map<String, Object?> map, String key) {
  if (!map.containsKey(key)) throw FormatException('Missing field "$key"');
  return map[key];
}

String jsonString(Object? value) {
  if (value is String) return value;
  throw FormatException('Expected a string, got ${value.runtimeType}');
}

String? jsonStringOrNull(Object? value) =>
    value == null ? null : jsonString(value);

bool jsonBool(Object? value) {
  if (value is bool) return value;
  throw FormatException('Expected a bool, got ${value.runtimeType}');
}

/// A `#[serde(default)]` bool field: absent means [fallback].
bool jsonBoolOr(Object? value, bool fallback) =>
    value == null ? fallback : jsonBool(value);

int jsonInt(Object? value) {
  if (value is int) return value;
  if (value is num) return value.toInt();
  throw FormatException('Expected an integer, got ${value.runtimeType}');
}

double jsonDouble(Object? value) {
  if (value is num) return value.toDouble();
  throw FormatException('Expected a number, got ${value.runtimeType}');
}

List<Object?> jsonList(Object? value) {
  if (value is List) return value;
  throw FormatException('Expected a list, got ${value.runtimeType}');
}

List<T> jsonListOf<T>(Object? value, T Function(Object? item) parse) =>
    [for (final item in jsonList(value)) parse(item)];
