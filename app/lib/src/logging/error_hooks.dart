// Uncaught error capture (the Scratch log_buffer.dart pattern): FlutterError.onError and
// PlatformDispatcher.onError are chained (the handler that was there before still runs), and
// [runGuarded] adds the zone handler for async errors outside both. Each error becomes one
// level "error" line with target "uncaught" and fields.zone, in the app's log.

import 'dart:async';

import 'package:flutter/foundation.dart';

import 'app_log.dart';

void Function(FlutterErrorDetails)? _flutterHook;
bool Function(Object, StackTrace)? _platformHook;

/// Logs one uncaught error. [zone] says where it was caught: `flutter`, `platform` or `zone`.
void logUncaught(Object error, StackTrace? stack, String zone, {AppLogger? log}) {
  try {
    (log ?? AppLog.instance).log(
      LogLevel.error,
      'uncaught',
      'Uncaught error',
      error: error,
      stack: stack,
      fields: {'zone': zone},
    );
  } catch (_) {}
}

/// Chains [FlutterError.onError] and [PlatformDispatcher.onError] into the log. Calling it
/// again while these hooks are still in place does nothing.
void installErrorHooks({AppLogger? log}) {
  if (_flutterHook == null || !identical(FlutterError.onError, _flutterHook)) {
    final previous = FlutterError.onError;
    _flutterHook = (details) {
      logUncaught(details.exception, details.stack, 'flutter', log: log);
      if (previous != null) {
        previous(details);
      } else {
        FlutterError.presentError(details);
      }
    };
    FlutterError.onError = _flutterHook;
  }
  if (_platformHook == null || !identical(PlatformDispatcher.instance.onError, _platformHook)) {
    final previous = PlatformDispatcher.instance.onError;
    _platformHook = (error, stack) {
      logUncaught(error, stack, 'platform', log: log);
      return previous?.call(error, stack) ?? false;
    };
    PlatformDispatcher.instance.onError = _platformHook;
  }
}

/// Runs [body] in a zone whose uncaught async errors go to the log and then to the console.
void runGuarded(FutureOr<void> Function() body, {AppLogger? log}) {
  runZonedGuarded<Future<void>>(
    () async => body(),
    (error, stack) {
      logUncaught(error, stack, 'zone', log: log);
      debugPrint('Uncaught zone error: $error\n$stack');
    },
  );
}
