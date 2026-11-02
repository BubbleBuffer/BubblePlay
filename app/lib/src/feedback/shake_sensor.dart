import 'package:sensors_plus/sensors_plus.dart';

import 'shake_detector.dart';

/// The real accelerometer as shake samples (gravity included). Android only: the caller decides.
Stream<ShakeSample> accelerometerSamples() => accelerometerEventStream(
      samplingPeriod: SensorInterval.gameInterval,
    ).map((e) => ShakeSample(e.x, e.y, e.z, DateTime.now()));
