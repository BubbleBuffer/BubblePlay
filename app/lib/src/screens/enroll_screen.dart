import 'package:flutter/widgets.dart' hide Action, Flow;

import '../model/model.dart';

/// First run and key unlock: create a key, import a key file, enter a passphrase.
class EnrollScreen extends StatelessWidget {
  const EnrollScreen({super.key, required this.state, required this.step});

  final AppState state;
  final EnrollStep step;

  @override
  Widget build(BuildContext context) => const SizedBox.shrink();
}
