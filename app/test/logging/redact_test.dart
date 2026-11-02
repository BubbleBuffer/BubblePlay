import 'package:bubbleplay_app/src/logging/redact.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('isSensitiveKey (rule 1)', () {
    test('names that must be redacted', () {
      for (final key in [
        'password',
        'Password',
        'passphrase',
        'passwd',
        'secret',
        'client_secret',
        'token',
        'accessToken',
        'access-token',
        'api.key',
        'apiKey',
        'key',
        'keys',
        'proof',
        'ticket',
        'pairing_code',
        'pairingCode',
        'code',
        'psk',
        'signature',
        'nonce_secret',
        'auth',
        'authorization',
        'cookie',
        'credentials',
      ]) {
        expect(isSensitiveKey(key), isTrue, reason: key);
      }
    });

    test('names that are ids or plain words stay', () {
      for (final key in [
        'keyboard',
        'keyframe',
        'keycode',
        'keyid',
        'key_id',
        'device_key_id',
        'deviceKeyId',
        'error',
        'stack',
        'codec',
        'monitor',
        'rtt_ms',
        'dropped',
        'zone',
        'action',
      ]) {
        expect(isSensitiveKey(key), isFalse, reason: key);
      }
    });
  });

  group('redactField', () {
    test('a sensitive key replaces the whole value, whatever it is', () {
      expect(redactField('password', 'hunter2'), redacted);
      expect(redactField('pairing_code', 123456), redacted);
      expect(redactField('token', {'a': 'b'}), redacted);
      expect(redactField('proof', ['x', 'y']), redacted);
    });

    test('nested maps and lists are walked', () {
      final out = redactField('fields', {
        'outer': {'secret': 'abc', 'note': 'ok'},
        'list': [
          {'ticket': 'zzz'},
          'Bearer abc.def',
        ],
      }) as Map;
      expect(out['outer'], {'secret': redacted, 'note': 'ok'});
      expect(out['list'], [
        {'ticket': redacted},
        'Bearer $redacted',
      ]);
    });

    test('other values pass through', () {
      expect(redactField('rtt_ms', 42), 42);
      expect(redactField('ok', true), true);
      expect(redactField('error', null), null);
    });
  });

  group('redactText', () {
    test('rule 2: URL query and fragment parameters', () {
      expect(
        redactText('GET https://r.example/pair?code=123456&name=pc&token=abc.def#key=zzz'),
        'GET https://r.example/pair?code=$redacted&name=pc&token=$redacted#key=$redacted',
      );
      expect(
        redactText('https://r.example/x?sig=AAAA&ticket=BBBB&proof=CCCC'),
        'https://r.example/x?sig=$redacted&ticket=$redacted&proof=$redacted',
      );
      expect(redactText('https://r.example/x?page=2'), 'https://r.example/x?page=2');
    });

    test('rule 3: Bearer and Authorization', () {
      expect(redactText('sent Bearer abc.def-123 ok'), 'sent Bearer $redacted ok');
      expect(redactText('bearer   xyz'), 'Bearer $redacted');
      expect(
        redactText('headers:\nAuthorization: Basic dXNlcjpwYXNz\nAccept: */*'),
        'headers:\nAuthorization: $redacted\nAccept: */*',
      );
    });

    test('rule 4: key=value, key: value and JSON pairs in free text', () {
      expect(redactText('login password=hunter2 ok'), 'login password=$redacted ok');
      expect(redactText('passphrase: correct-horse'), 'passphrase: $redacted');
      expect(redactText('{"token":"abc","name":"pc"}'), '{"token":"$redacted","name":"pc"}');
      expect(redactText('{"pairing_code": 123456}'), '{"pairing_code": "$redacted"}');
      expect(redactText("secret='two words' next"), 'secret=$redacted next');
      expect(redactText('keyboard=us rtt_ms=40'), 'keyboard=us rtt_ms=40');
    });

    test('rule 5: registered literals, longest first', () {
      expect(
        redactText('a hunter2-extra b hunter2', secrets: ['hunter2', 'hunter2-extra']),
        'a $redacted b $redacted',
      );
      // Too short to register: it would blot out ordinary text.
      expect(redactText('abc', secrets: ['abc']), 'abc');
    });

    test('rule 6: runs of 40 or more token characters', () {
      final blob = 'A' * 39;
      expect(redactText('x $blob y'), 'x $blob y');
      expect(redactText('x ${blob}B y'), 'x $redactedBlob y');
      expect(redactText('b64 ${'Zm9v/YmFy+' * 6}== end'), 'b64 $redactedBlob end');
    });

    test('rule 6 in a stack trace keeps long package paths but not blobs', () {
      const frame =
          '#0 Foo.bar (package:bubbleplay_app/src/screens/pairing/pair_confirm_view.dart:12:5)';
      expect(redactText(frame), contains(redactedBlob));
      expect(redactText(frame, stack: true), frame);
      expect(redactText('${'A1' * 30} at x', stack: true), '$redactedBlob at x');
    });

    test('ordinary text is untouched', () {
      const text = 'Connected to the PC in 120 ms (h264, 1920x1080)';
      expect(redactText(text), text);
    });

    test('never throws', () {
      expect(() => redactText('\u0000' * 5000 + 'password=x'), returnsNormally);
    });
  });
}
