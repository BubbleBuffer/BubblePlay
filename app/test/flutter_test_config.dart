// Real bundled typefaces and dependency icon fonts keep goldens deterministic.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

Future<void> testExecutable(FutureOr<void> Function() testMain) async {
  TestWidgetsFlutterBinding.ensureInitialized();
  const families = {
    'Bricolage Grotesque': ['BricolageGrotesque[opsz,wdth,wght].ttf'],
    'Atkinson Hyperlegible Next': [
      'AtkinsonHyperlegibleNext[wght].ttf',
      'AtkinsonHyperlegibleNext-Italic[wght].ttf',
    ],
    'Fragment Mono': ['FragmentMono-Regular.ttf', 'FragmentMono-Italic.ttf'],
  };
  for (final MapEntry(key: family, value: files) in families.entries) {
    final loader = FontLoader(family);
    for (final file in files) {
      final bytes = File('assets/fonts/$file').readAsBytesSync();
      loader.addFont(Future.value(ByteData.sublistView(bytes)));
    }
    await loader.load();
  }
  await _loadPackageFonts('lucide_icons_flutter');
  await testMain();
}

// flutter test does not register dependency fonts automatically. Match the
// package-qualified family names used by IconData, as agent-kit's loader does.
Future<void> _loadPackageFonts(String package) async {
  final root = _packageRoot(package);
  if (root == null) throw StateError('Font package not resolved: $package');
  final loaders = <String, FontLoader>{};
  String? family;
  final familyLine = RegExp(r'^\s*-\s*family:\s*(\S+)');
  final assetLine = RegExp(r'^\s*-?\s*asset:\s*(\S+)');
  for (final line in File('$root/pubspec.yaml').readAsLinesSync()) {
    final f = familyLine.firstMatch(line);
    if (f != null) {
      family = f.group(1)!.replaceAll(RegExp('[\'"]'), '');
      continue;
    }
    final a = assetLine.firstMatch(line);
    if (a != null && family != null) {
      final name = 'packages/$package/$family';
      final path = a.group(1)!.replaceAll(RegExp('[\'"]'), '');
      loaders
          .putIfAbsent(name, () => FontLoader(name))
          .addFont(
            Future.value(
              ByteData.sublistView(File('$root/$path').readAsBytesSync()),
            ),
          );
    }
  }
  if (loaders.isEmpty) throw StateError('No font families found in $package');
  for (final loader in loaders.values) {
    await loader.load();
  }
}

// Workspace members resolve packages through the nearest ancestor config.
String? _packageRoot(String package) {
  var dir = Directory.current.absolute;
  while (true) {
    final config = File('${dir.path}/.dart_tool/package_config.json');
    if (config.existsSync()) {
      final json =
          jsonDecode(config.readAsStringSync()) as Map<String, Object?>;
      for (final entry in json['packages']! as List<Object?>) {
        final p = entry! as Map<String, Object?>;
        if (p['name'] == package) {
          var root = config.uri.resolve(p['rootUri']! as String);
          if (!root.path.endsWith('/')) {
            root = root.replace(path: '${root.path}/');
          }
          return File.fromUri(root.resolve('pubspec.yaml')).parent.path;
        }
      }
      return null;
    }
    final parent = dir.parent;
    if (parent.path == dir.path) return null;
    dir = parent;
  }
}
