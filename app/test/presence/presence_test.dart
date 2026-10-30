import 'dart:convert';

import 'package:bubbleplay_app/src/app_controller.dart';
import 'package:bubbleplay_app/src/backend/fake_channel.dart';
import 'package:bubbleplay_app/src/model/model.dart';
import 'package:bubbleplay_app/src/presence/presence_service.dart';
import 'package:flutter_test/flutter_test.dart';

import '../support/harness.dart';

RemotePc pc(String id, String relay) => RemotePc.fromJson({
      'id': id,
      'name': 'PC $id',
      'kind': 'Desktop',
      'relay': relay,
      'online': false,
      'allowed': 'View',
    });

void offline(Map<String, Object?> state) {
  for (final p in (state['pcs'] as List).cast<Map<String, Object?>>()) {
    p['online'] = false;
  }
}

String id(int n) => 'AAAA-BBBB-CCCC-${n.toString().padLeft(4, '0')}';

void main() {
  group('PresenceService', () {
    test('groups by relay origin and chunks by 20', () async {
      final urls = <Uri>[];
      final service = PresenceService(fetcher: (url) async {
        urls.add(url);
        final ids = url.queryParameters['ids']!.split(',');
        return PresenceReply(200, jsonEncode({for (final i in ids) i: true}));
      });
      final pcs = [
        for (var i = 0; i < 25; i++) pc(id(i), 'https://a.example/x'),
        pc(id(100), 'https://b.example'),
        pc(id(101), ''),
      ];
      expect(await service.poll(pcs), isTrue);
      expect(urls.length, 3);
      expect(urls.map((u) => u.origin).toSet(), {'https://a.example', 'https://b.example'});
      expect(urls.every((u) => u.path == '/presence'), isTrue);
      expect(urls.every((u) => u.queryParameters['ids']!.split(',').length <= 20), isTrue);
      expect(service.online(id(3)), isTrue);
      expect(service.online(id(101)), isNull);
    });

    test('errors keep the last value and never show online', () async {
      var mode = 'ok';
      final service = PresenceService(fetcher: (url) async {
        if (mode == 'throw') throw Exception('down');
        if (mode == '500') return const PresenceReply(500, '{}');
        if (mode == 'junk') return const PresenceReply(200, 'nope');
        return PresenceReply(200, jsonEncode({url.queryParameters['ids']: mode == 'ok'}));
      });
      final pcs = [pc(id(1), 'https://a.example')];
      await service.poll(pcs);
      expect(service.online(id(1)), isTrue);
      for (final m in ['throw', '500', 'junk']) {
        mode = m;
        expect(await service.poll(pcs), isFalse);
        expect(service.online(id(1)), isTrue, reason: m);
      }
      final fresh = PresenceService(fetcher: (_) async => throw Exception('down'));
      await fresh.poll(pcs);
      expect(fresh.online(id(1)), isNull);
    });

    test('429 backs off for Retry-After, then asks again', () async {
      var now = DateTime(2026, 1, 1);
      var calls = 0;
      var status = 429;
      final service = PresenceService(
        now: () => now,
        fetcher: (url) async {
          calls++;
          return status == 429
              ? const PresenceReply(429, '', retryAfter: 90)
              : PresenceReply(200, jsonEncode({url.queryParameters['ids']: true}));
        },
      );
      final pcs = [pc(id(1), 'https://a.example')];
      await service.poll(pcs);
      expect(calls, 1);
      now = now.add(const Duration(seconds: 60));
      await service.poll(pcs);
      expect(calls, 1);
      expect(service.backedOffUntil('https://a.example'), isNotNull);
      now = now.add(const Duration(seconds: 31));
      status = 200;
      await service.poll(pcs);
      expect(calls, 2);
      expect(service.online(id(1)), isTrue);
    });
  });

  group('AppController presence', () {
    test('overlays online on the snapshot and polls while the list shows', () async {
      var polls = 0;
      final service = PresenceService(fetcher: (url) async {
        polls++;
        return PresenceReply(200, jsonEncode({url.queryParameters['ids']: true}));
      });
      final channel = FakeChannel(contractState('home', offline));
      final controller = AppController(
        channel,
        presence: service,
        presenceInterval: const Duration(milliseconds: 40),
      );
      addTearDown(controller.dispose);
      await controller.refresh();
      expect(controller.state!.pcs.first.online, isFalse);
      await Future<void>.delayed(const Duration(milliseconds: 20));
      expect(controller.state!.pcs.first.online, isTrue);
      await Future<void>.delayed(const Duration(milliseconds: 100));
      expect(polls, greaterThanOrEqualTo(2));

      controller.setForeground(false);
      await Future<void>.delayed(const Duration(milliseconds: 10));
      final paused = polls;
      await Future<void>.delayed(const Duration(milliseconds: 120));
      expect(polls, paused);
      controller.setForeground(true);
      await Future<void>.delayed(const Duration(milliseconds: 10));
      expect(polls, greaterThan(paused));

      channel.push(contractState('session'));
      await Future<void>.delayed(const Duration(milliseconds: 20));
      final inSession = polls;
      await Future<void>.delayed(const Duration(milliseconds: 120));
      expect(polls, inSession);
    });

    test('without a service the snapshot is untouched', () async {
      final controller = AppController(FakeChannel(contractState('home', offline)));
      addTearDown(controller.dispose);
      await controller.refresh();
      await Future<void>.delayed(const Duration(milliseconds: 20));
      expect(controller.state!.pcs.first.online, isFalse);
    });
  });
}
