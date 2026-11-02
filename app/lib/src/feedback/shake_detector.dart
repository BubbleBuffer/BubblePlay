// Shake to report a problem (Android). [ShakeDetector] is the pure logic; [ShakeService] ties it
// to an accelerometer stream, only while the app is in the foreground, and stops it on dispose.

import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/widgets.dart';

/// Standard gravity, m/s^2: samples are divided by it so the threshold reads in g.
const _gravity = 9.80665;

/// A sample counts as a jolt when the total force is this many g or more (rest is 1 g).
const shakeThresholdG = 2.7;

/// This many jolts inside [shakeWindow] make a shake.
const shakeJolts = 3;

/// The jolts must all fall inside this span.
const shakeWindow = Duration(milliseconds: 1000);

/// After a shake, further shakes are ignored for this long.
const shakeDebounce = Duration(seconds: 2);

/// Two over-threshold samples closer than this are one jolt (a sensor reports a swing many times).
const shakeJoltGap = Duration(milliseconds: 150);

/// One accelerometer reading, gravity included, in m/s^2.
class ShakeSample {
  const ShakeSample(this.x, this.y, this.z, this.time);

  final double x;
  final double y;
  final double z;
  final DateTime time;

  double get g => math.sqrt(x * x + y * y + z * z) / _gravity;
}

class ShakeDetector {
  ShakeDetector(this.onShake);

  final void Function() onShake;

  final List<DateTime> _jolts = [];
  DateTime? _lastJolt;
  DateTime? _lastShake;

  /// Feeds one reading; calls [onShake] when it completes a shake.
  void add(ShakeSample sample) {
    final now = sample.time;
    final lastShake = _lastShake;
    if (lastShake != null && now.difference(lastShake) < shakeDebounce) return;
    if (sample.g < shakeThresholdG) return;
    final lastJolt = _lastJolt;
    if (lastJolt != null && now.difference(lastJolt) < shakeJoltGap) return;
    _lastJolt = now;
    _jolts.add(now);
    _jolts.removeWhere((t) => now.difference(t) > shakeWindow);
    if (_jolts.length >= shakeJolts) {
      _jolts.clear();
      _lastShake = now;
      onShake();
    }
  }

  /// Forgets the jolts seen so far (the app went to the background).
  void reset() {
    _jolts.clear();
    _lastJolt = null;
  }
}

/// Listens to [samples] while the app is resumed. Null [samples] (not Android, no sensor) makes
/// it do nothing. Call [dispose] to stop for good.
class ShakeService with WidgetsBindingObserver {
  ShakeService({
    required this.samples,
    required this.onShake,
    this.allowed = _always,
    WidgetsBinding? binding,
  }) : _binding = binding ?? WidgetsBinding.instance {
    _binding.addObserver(this);
    _foreground = _binding.lifecycleState == null || _binding.lifecycleState == AppLifecycleState.resumed;
    _sync();
  }

  static bool _always() => true;

  /// Opens the accelerometer stream; called each time listening starts.
  final Stream<ShakeSample> Function()? samples;

  /// Called on a shake while [allowed] is true.
  final void Function() onShake;

  /// False while a shake must be ignored (a remote session is showing).
  final bool Function() allowed;

  final WidgetsBinding _binding;
  late final ShakeDetector _detector = ShakeDetector(_fire);
  StreamSubscription<ShakeSample>? _subscription;
  bool _foreground = true;
  bool _disposed = false;

  /// True while the sensor stream is being listened to.
  bool get listening => _subscription != null;

  void _fire() {
    if (_disposed || !allowed()) return;
    onShake();
  }

  void _sync() {
    final open = samples;
    if (_foreground && !_disposed && open != null) {
      _subscription ??= open().listen(_detector.add, onError: (Object _) {});
    } else {
      _subscription?.cancel();
      _subscription = null;
      _detector.reset();
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    _foreground = state == AppLifecycleState.resumed;
    _sync();
  }

  void dispose() {
    _disposed = true;
    _binding.removeObserver(this);
    _sync();
  }
}
