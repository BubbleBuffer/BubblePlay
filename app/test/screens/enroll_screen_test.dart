import 'package:bb_ui/bb_ui.dart';
import 'package:bubbleplay_app/src/platform/key_files.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

/// The unlock step for this device's own stored key.
Object? ownKeyState() => contractState('enroll_unlock_clean', (state) {
      (state['flow'] as Map)['Enroll'] = <String, Object?>{
        'ImportUnlock': <String, Object?>{'error': null, 'file_name': 'device-key.bpkey'},
      };
    });

/// Text widgets showing exactly [text]; EditableText (a field's own value) is not one.
Finder textShown(String text) =>
    find.byWidgetPredicate((widget) => widget is Text && widget.data == text);

const String savedKeyFile = 'bubbleplay-device-key.bpkey';
const String fixtureKeyPath =
    r'C:\Users\me\AppData\Local\BubblePlay\viewer\device-key.bpkey';

void main() {
  group('choose a key on desktop', () {
    testWidgets('offers Create key and Import key file', (tester) async {
      await pumpApp(tester, contractState('first_run'));
      expect(find.text('Set up this device'), findsOneWidget);
      expect(find.text('Create key'), findsOneWidget);
      expect(find.text('Import key file'), findsOneWidget);
    });

    testWidgets('Create key sends CreateKey', (tester) async {
      final app = await pumpApp(tester, contractState('first_run'));
      await tester.tap(find.text('Create key'));
      await tester.pump();
      expect(app.actions.single, 'CreateKey');
    });

    testWidgets('Import key file sends the picked file text', (tester) async {
      final app = await pumpApp(tester, contractState('first_run'));
      app.keyFiles.picked = const PickedKeyFile(name: 'a.bpkey', text: 'T');
      await tester.tap(find.text('Import key file'));
      await tester.pumpAndSettle();
      expect(app.actions.single, {
        'ImportKeyText': {'file_name': 'a.bpkey', 'text': 'T'},
      });
    });

    testWidgets('Import key file sends nothing when no file is picked', (tester) async {
      final app = await pumpApp(tester, contractState('first_run'));
      await tester.tap(find.text('Import key file'));
      await tester.pumpAndSettle();
      expect(app.actions, isEmpty);
    });
  });

  group('choose a key on a phone', () {
    testWidgets('shows the step and sends CreateKey', (tester) async {
      final app = await pumpApp(
        tester,
        contractState('first_run'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Set up this device'), findsOneWidget);
      expect(find.text('Import key file'), findsOneWidget);
      await tester.tap(find.text('Create key'));
      await tester.pump();
      expect(app.actions.single, 'CreateKey');
    });
  });

  group('key created on desktop', () {
    testWidgets('shows the device ID, where the key is stored, and both buttons', (tester) async {
      await pumpApp(tester, contractState('enroll_created'));
      expect(find.text('Key created'), findsOneWidget);
      expect(find.text('AB12-CD34-EF56-GH78'), findsOneWidget);
      expect(find.text('Stored at'), findsOneWidget);
      expect(find.text(fixtureKeyPath), findsOneWidget);
      expect(find.text('Export key'), findsOneWidget);
      expect(find.text('Continue'), findsOneWidget);
      expect(find.byType(NoticeBanner), findsNothing);
      expect(find.text('Key file saved.'), findsNothing);
    });

    testWidgets('Export key saves the key text and says so', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_created'));
      await tester.tap(find.text('Export key'));
      await tester.pumpAndSettle();
      expect(app.keyFiles.saved, {savedKeyFile: app.channel.keyText});
      expect(find.text('Key file saved.'), findsOneWidget);
    });

    testWidgets('Continue sends CancelFlow', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_created'));
      await tester.tap(find.text('Continue'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });
  });

  group('key created on a phone', () {
    testWidgets('shows the ID, the stored path and both buttons', (tester) async {
      await pumpApp(
        tester,
        contractState('enroll_created'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Key created'), findsOneWidget);
      expect(find.text('AB12-CD34-EF56-GH78'), findsOneWidget);
      expect(find.text(fixtureKeyPath), findsOneWidget);
      expect(find.text('Export key'), findsOneWidget);
      expect(find.text('Continue'), findsOneWidget);
    });
  });

  group('unlock an imported key', () {
    testWidgets('shows the file, the field and Cancel, with no error', (tester) async {
      await pumpApp(tester, contractState('enroll_unlock_clean'));
      expect(find.text('Import key file'), findsOneWidget);
      expect(find.text('viewer.bpkey is protected with a passphrase.'), findsOneWidget);
      expect(find.text('The passphrase is only used on this device.'), findsOneWidget);
      expect(find.text('Cancel'), findsOneWidget);
      expect(find.byType(NoticeBanner), findsNothing);
    });

    testWidgets('typing a passphrase and tapping Unlock sends UnlockImport', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_unlock_clean'));
      await tester.enterText(find.byType(EditableText), 'correct horse');
      await tester.pump();
      await tester.tap(find.text('Unlock'));
      await tester.pump();
      expect(app.actions.single, {
        'UnlockImport': {'passphrase': 'correct horse'},
      });
      expect(textShown('correct horse'), findsNothing);
    });

    testWidgets('the field hides what is typed', (tester) async {
      await pumpApp(tester, contractState('enroll_unlock_clean'));
      await tester.enterText(find.byType(EditableText), 'secret-pass');
      await tester.pump();
      final field = tester.widget<EditableText>(find.byType(EditableText));
      expect(field.obscureText, isTrue);
      expect(textShown('secret-pass'), findsNothing);
      expect(field.controller.text, 'secret-pass');
    });

    testWidgets('Unlock does nothing while the field is empty', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_unlock_clean'));
      final button = tester.widget<PrimaryButton>(
        find.widgetWithText(PrimaryButton, 'Unlock'),
      );
      expect(button.onTap, isNull);
      await tester.tap(find.text('Unlock'), warnIfMissed: false);
      await tester.pump();
      expect(app.actions, isEmpty);
    });

    testWidgets('an error from the core is shown above the field', (tester) async {
      await pumpApp(tester, contractState('enroll_unlock'));
      expect(find.text('That passphrase did not open the key.'), findsOneWidget);
      final banner = tester.widget<NoticeBanner>(find.byType(NoticeBanner));
      expect(banner.kind, NoticeKind.danger);
    });

    testWidgets('Cancel sends CancelFlow', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_unlock_clean'));
      await tester.tap(find.text('Cancel'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });
  });

  group('unlock this device\'s own key', () {
    testWidgets('has no Cancel button and still unlocks', (tester) async {
      final app = await pumpApp(tester, ownKeyState());
      expect(find.text('Unlock your key'), findsOneWidget);
      expect(find.text('Cancel'), findsNothing);
      await tester.enterText(find.byType(EditableText), 'pw');
      await tester.pump();
      await tester.tap(find.text('Unlock'));
      await tester.pump();
      expect(app.actions.single, {
        'UnlockImport': {'passphrase': 'pw'},
      });
    });

    testWidgets('works on a phone', (tester) async {
      await pumpApp(
        tester,
        contractState('enroll_unlock'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Import key file'), findsOneWidget);
      expect(find.text('That passphrase did not open the key.'), findsOneWidget);
      expect(find.text('Cancel'), findsOneWidget);
    });
  });

  group('a key file that is not accepted', () {
    testWidgets('names the file, gives the reason and offers both buttons', (tester) async {
      await pumpApp(tester, contractState('enroll_refused'));
      expect(find.text('Key file not accepted'), findsOneWidget);
      expect(find.text('notes.txt'), findsOneWidget);
      expect(find.text('This is not a BubblePlay key file.'), findsOneWidget);
      expect(find.text('Choose another file'), findsOneWidget);
      expect(find.text('Back'), findsOneWidget);
    });

    testWidgets('Choose another file imports the newly picked file', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_refused'));
      app.keyFiles.picked = const PickedKeyFile(name: 'b.bpkey', text: 'K');
      await tester.tap(find.text('Choose another file'));
      await tester.pumpAndSettle();
      expect(app.actions.single, {
        'ImportKeyText': {'file_name': 'b.bpkey', 'text': 'K'},
      });
    });

    testWidgets('Back sends CancelFlow', (tester) async {
      final app = await pumpApp(tester, contractState('enroll_refused'));
      await tester.tap(find.text('Back'));
      await tester.pump();
      expect(app.actions.single, 'CancelFlow');
    });

    testWidgets('shows both buttons on a phone', (tester) async {
      await pumpApp(
        tester,
        contractState('enroll_refused'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('notes.txt'), findsOneWidget);
      expect(find.text('This is not a BubblePlay key file.'), findsOneWidget);
      expect(find.text('Choose another file'), findsOneWidget);
      expect(find.text('Back'), findsOneWidget);
    });
  });

  group('through the gate', () {
    testWidgets('the first_run snapshot opens the key setup', (tester) async {
      await pumpApp(tester, contractState('first_run'));
      expect(find.text('Set up this device'), findsOneWidget);
      expect(find.text('Create key'), findsOneWidget);
    });

    testWidgets('the first_run snapshot opens the key setup on a phone too', (tester) async {
      await pumpApp(
        tester,
        contractState('first_run'),
        screen: Screen.phone,
        windows: false,
      );
      expect(find.text('Set up this device'), findsOneWidget);
      expect(find.text('Import key file'), findsOneWidget);
    });
  });
}
