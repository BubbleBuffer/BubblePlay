import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import 'backend/channel.dart';
import 'logging/app_log.dart';
import 'model/model.dart' as model;
import 'presence/presence_service.dart';

/// The app's one source of truth: the latest [model.AppState] from the core, and the way to send
/// it actions. Screens read [state] and call [act]; they never hold copies.
class AppController extends ChangeNotifier {
  /// [presence], when given, fills in `RemotePc.online` from the relays (the cores report false).
  AppController(
    this._channel, {
    PresenceService? presence,
    this.presenceInterval = const Duration(seconds: 30),
  }) : _presence = presence { // ignore: prefer_initializing_formals
    _subscription = _channel.changes.listen((_) => refresh());
  }

  final AppChannel _channel;
  late final StreamSubscription<void> _subscription;
  final PresenceService? _presence;

  /// How often the PC list is polled while shown.
  final Duration presenceInterval;
  Timer? _presenceTimer;
  bool _foreground = true;
  bool _polling = false;
  model.AppState? _core;
  model.AppState? _state;
  Object? _error;
  bool _disposed = false;
  String? _lastErrorText;
  String? _loggedPc;

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
      _core = next;
      _state = _overlay(next);
      _error = null;
      _lastErrorText = null;
      _logContext(next);
    } catch (e) {
      if (_disposed) return;
      _error = e;
      // Once per distinct error: a core that stays down must not fill the log on every poll.
      final text = e.toString();
      if (text != _lastErrorText) {
        _lastErrorText = text;
        AppLog.error('AppController', 'Could not read the core snapshot', error: e);
      }
    }
    notifyListeners();
    _schedulePresence();
  }

  /// Keeps the log's device and PC context in step with the snapshot, and notes when a session
  /// screen opens or closes.
  void _logContext(model.AppState next) {
    AppLog.setDevice(next.thisDevice.id);
    final pc = next.session?.pcId;
    if (pc == _loggedPc) return;
    if (pc != null) {
      AppLog.setContext(pc: pc);
      AppLog.info('AppController', 'Session started');
    } else {
      AppLog.info('AppController', 'Session ended');
      AppLog.setContext();
    }
    _loggedPc = pc;
  }

  /// Tells the controller whether the app is in the foreground; polling pauses in the background.
  void setForeground(bool value) {
    if (_foreground == value) return;
    _foreground = value;
    _schedulePresence();
  }

  bool get _listShown {
    final s = _core;
    return s != null && s.session == null && s.flow is! model.Enroll;
  }

  model.AppState _overlay(model.AppState s) {
    final presence = _presence;
    if (presence == null) return s;
    return model.AppState(
      thisDevice: s.thisDevice,
      host: s.host,
      flow: s.flow,
      session: s.session,
      pcs: [
        for (final pc in s.pcs)
          model.RemotePc(
            id: pc.id,
            name: pc.name,
            kind: pc.kind,
            relay: pc.relay,
            online: presence.online(pc.id) ?? false,
            lastMode: pc.lastMode,
            lastSeen: pc.lastSeen,
            allowed: pc.allowed,
            vouchRequest: pc.vouchRequest,
            problem: pc.problem,
            defaultMonitor: pc.defaultMonitor,
          ),
      ],
    );
  }

  /// Starts or stops the poll loop to match what is on screen.
  void _schedulePresence() {
    if (_presence == null || _disposed) return;
    if (!(_listShown && _foreground)) {
      _presenceTimer?.cancel();
      _presenceTimer = null;
      return;
    }
    if (_presenceTimer == null) {
      _presenceTimer = Timer.periodic(presenceInterval, (_) => _pollPresence());
      _pollPresence();
    }
  }

  Future<void> _pollPresence() async {
    final presence = _presence;
    final core = _core;
    if (presence == null || core == null || _polling || !_listShown || !_foreground) return;
    _polling = true;
    try {
      final changed = await presence.poll(core.pcs);
      final latest = _core;
      if (_disposed || !changed || latest == null) return;
      _state = _overlay(latest);
      notifyListeners();
    } finally {
      _polling = false;
    }
  }

  /// Sends [action] to the core. Its effect arrives as a later snapshot.
  Future<void> act(model.Action action) async {
    try {
      await _channel.act(jsonEncode(action.toJson()));
    } catch (e) {
      _error = e;
      // The action's class only: its fields can carry pairing codes and links.
      AppLog.error('AppController', 'The core refused an action', error: e, fields: {'action': action.runtimeType.toString()});
      notifyListeners();
    }
  }

  /// Hides the failure notice (a later good snapshot clears it too).
  void dismissError() {
    if (_error == null) return;
    _error = null;
    notifyListeners();
  }

  /// The stored key as portable text, for saving to a file.
  Future<String> exportKey() => _channel.exportKey();

  @override
  void dispose() {
    _disposed = true;
    _presenceTimer?.cancel();
    _subscription.cancel();
    super.dispose();
  }
}
