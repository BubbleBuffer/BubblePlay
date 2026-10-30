import 'dart:convert';
import 'dart:io';

import 'package:bubbleplay_app/src/model/model.dart';
import 'package:flutter_test/flutter_test.dart';

/// Reads one of the fixtures generated from `client-win/src/model.rs`.
Map<String, Object?> loadFixture(String name) =>
    jsonDecode(File('test/fixtures/contract/$name.json').readAsStringSync())
        as Map<String, Object?>;

/// Deep equality that treats an int and a double of the same value as equal
/// (`jsonEncode(17.0)` is "17.0", the fixture may say "17").
bool deepEquals(Object? a, Object? b) {
  if (a is Map && b is Map) {
    if (a.length != b.length) return false;
    for (final key in a.keys) {
      if (!b.containsKey(key)) return false;
      if (!deepEquals(a[key], b[key])) return false;
    }
    return true;
  }
  if (a is List && b is List) {
    if (a.length != b.length) return false;
    for (var i = 0; i < a.length; i++) {
      if (!deepEquals(a[i], b[i])) return false;
    }
    return true;
  }
  if (a is num && b is num) return a == b;
  return a == b;
}

/// A readable description of the first difference, for test failures.
String describeDiff(Object? expected, Object? actual, [String path = '']) {
  if (expected is Map && actual is Map) {
    for (final key in expected.keys) {
      if (!actual.containsKey(key)) return '$path.$key is missing';
      final diff = describeDiff(expected[key], actual[key], '$path.$key');
      if (diff.isNotEmpty) return diff;
    }
    for (final key in actual.keys) {
      if (!expected.containsKey(key)) return '$path.$key is extra';
    }
    return '';
  }
  if (expected is List && actual is List) {
    if (expected.length != actual.length) {
      return '$path has ${actual.length} entries, expected ${expected.length}';
    }
    for (var i = 0; i < expected.length; i++) {
      final diff = describeDiff(expected[i], actual[i], '$path[$i]');
      if (diff.isNotEmpty) return diff;
    }
    return '';
  }
  if (!deepEquals(expected, actual)) {
    return '$path was ${jsonEncode(actual)}, expected ${jsonEncode(expected)}';
  }
  return '';
}

void main() {
  final states = loadFixture('states');
  final actions = loadFixture('actions');

  group('AppState', () {
    test('states.json has every named snapshot', () {
      expect(states.length, greaterThanOrEqualTo(33));
    });

    for (final entry in states.entries) {
      test('${entry.key} round-trips', () {
        final Object? expected = jsonDecode(jsonEncode(entry.value));
        final actual = AppState.fromJson(expected).toJson();
        final diff = describeDiff(expected, actual);
        expect(diff, isEmpty, reason: diff);
      });
    }
  });

  group('Action', () {
    test('actions.json has every named action', () {
      expect(actions.length, greaterThanOrEqualTo(36));
    });

    for (final entry in actions.entries) {
      test('${entry.key} round-trips', () {
        final Object? expected = jsonDecode(jsonEncode(entry.value));
        final actual = Action.fromJson(expected).toJson();
        final diff = describeDiff(expected, actual);
        expect(diff, isEmpty, reason: diff);
      });
    }

    test('an unknown variant throws', () {
      expect(
        () => Action.fromJson(<String, Object?>{'Nope': 1}),
        throwsFormatException,
      );
    });
  });
}
