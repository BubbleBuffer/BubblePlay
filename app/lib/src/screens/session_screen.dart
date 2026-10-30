import 'package:flutter/widgets.dart' hide Action, Flow;

import '../model/model.dart';

/// The placeholder session: the session chrome over a black area, no video yet.
class SessionScreen extends StatelessWidget {
  const SessionScreen({super.key, required this.state, required this.session});

  final AppState state;
  final Session session;

  @override
  Widget build(BuildContext context) => const SizedBox.shrink();
}
