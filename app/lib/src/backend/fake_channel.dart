import 'dart:async';
import 'dart:convert';

import 'channel.dart';

/// A channel without a core, for widget tests and goldens: it serves a fixed snapshot and
/// records every action. [push] replaces the snapshot and fires a change.
class FakeChannel implements AppChannel {
  FakeChannel(Object? state) : _state = jsonEncode(state);

  String _state;
  final List<String> actions = <String>[];
  final StreamController<void> _changes = StreamController<void>.broadcast();

  /// When set, [act] records the action and then throws this, like a core that failed it.
  Object? actError;

  /// The passphrase of every [exportKey] call, null when none was given.
  final List<String?> exportPassphrases = <String?>[];

  /// The key text [exportKey] returns.
  String keyText = '{"format":"bubbleplay-device-key"}';

  void push(Object? state) {
    _state = jsonEncode(state);
    _changes.add(null);
  }

  @override
  Future<String> snapshot() async => _state;

  @override
  Future<void> act(String actionJson) async {
    actions.add(actionJson);
    final error = actError;
    if (error != null) throw error;
  }

  @override
  Future<String> exportKey({String? passphrase}) async {
    exportPassphrases.add(passphrase);
    return passphrase == null ? keyText : '$keyText#sealed:$passphrase';
  }

  @override
  bool get sealsExport => true;

  @override
  Stream<void> get changes => _changes.stream;
}
