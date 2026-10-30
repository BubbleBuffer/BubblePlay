import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import 'backend/channel.dart';
import 'model/model.dart' as model;

/// The app's one source of truth: the latest [model.AppState] from the core, and the way to send
/// it actions. Screens read [state] and call [act]; they never hold copies.
class AppController extends ChangeNotifier {
  AppController(this._channel) {
    _subscription = _channel.changes.listen((_) => refresh());
  }

  final AppChannel _channel;
  late final StreamSubscription<void> _subscription;
  model.AppState? _state;
  Object? _error;
  bool _disposed = false;

  /// The latest snapshot; null only until the first one arrives.
  model.AppState? get state => _state;

  /// Set when the core could not be reached or answered something unreadable.
  Object? get error => _error;

  /// Reads a fresh snapshot.
  Future<void> refresh() async {
    try {
      final text = await _channel.snapshot();
      final next = model.AppState.fromJson(jsonDecode(text));
      if (_disposed) return;
      _state = next;
      _error = null;
    } catch (e) {
      if (_disposed) return;
      _error = e;
    }
    notifyListeners();
  }

  /// Sends [action] to the core. Its effect arrives as a later snapshot.
  Future<void> act(model.Action action) async {
    try {
      await _channel.act(jsonEncode(action.toJson()));
    } catch (e) {
      _error = e;
      notifyListeners();
    }
  }

  /// The stored key as portable text, for saving to a file.
  Future<String> exportKey() => _channel.exportKey();

  @override
  void dispose() {
    _disposed = true;
    _subscription.cancel();
    super.dispose();
  }
}
