import 'package:bubbleplay_app/src/logging/app_log.dart';
import 'package:bubbleplay_app/src/model/model.dart' as model;
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

void main() {
  group('an action the core fails', () {
    testWidgets('shows a notice, logs the cause and can be dismissed', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'), screen: Screen.phone, windows: false);
      expect(find.text('That did not work'), findsNothing);

      app.channel.actError = PlatformException(code: 'action_failed', message: 'The action failed (JSONException).');
      await app.controller.act(const model.CancelFlow());
      await tester.pump();

      expect(app.controller.error, isA<PlatformException>());
      expect(find.text('That did not work'), findsOneWidget);
      final logs = await AppLog.instance.reportLogs();
      final failure = logs.errors.where((l) => l['msg'] == 'The core refused an action');
      expect(failure, isNotEmpty);
      final fields = failure.last['fields'] as Map;
      expect(fields['action'], 'CancelFlow');
      expect(fields['error'], contains('action_failed'));

      await tester.tap(find.text('Dismiss'));
      await tester.pump();
      expect(find.text('That did not work'), findsNothing);
      expect(app.controller.error, isNull);
    });

    testWidgets('a good action leaves no notice', (tester) async {
      final app = await pumpApp(tester, contractState('pair_enter'), screen: Screen.phone, windows: false);
      await app.controller.act(const model.CancelFlow());
      await tester.pump();
      expect(find.text('That did not work'), findsNothing);
      expect(app.actions, hasLength(1));
    });
  });
}
