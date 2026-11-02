import 'package:flutter/widgets.dart';

/// Reports whether the app is in the foreground, so polling pauses while it is backgrounded.
class PresenceLifecycle with WidgetsBindingObserver {
  PresenceLifecycle(this.onForeground) {
    WidgetsBinding.instance.addObserver(this);
  }

  final void Function(bool foreground) onForeground;

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) =>
      onForeground(state == AppLifecycleState.resumed);

  void dispose() => WidgetsBinding.instance.removeObserver(this);
}
