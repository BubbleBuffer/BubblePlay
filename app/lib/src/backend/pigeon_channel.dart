import 'dart:async';

import 'channel.dart';
import 'pigeon.g.dart';

/// Android: the Kotlin core through Pigeon.
class PigeonChannel implements AppChannel, AppFlutterApi {
  PigeonChannel() : _host = AppHostApi() {
    AppFlutterApi.setUp(this);
  }

  final AppHostApi _host;
  final StreamController<void> _changes = StreamController<void>.broadcast();

  @override
  void changed() => _changes.add(null);

  @override
  Future<String> snapshot() => _host.snapshot();

  @override
  Future<void> act(String actionJson) => _host.act(actionJson);

  @override
  Future<String> exportKey() => _host.exportKey();

  @override
  Stream<void> get changes => _changes.stream;
}
