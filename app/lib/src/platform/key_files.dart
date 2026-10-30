import 'dart:convert';
import 'dart:typed_data';
import 'package:file_picker/file_picker.dart';

/// A key file the person chose to import.
class PickedKeyFile {
  const PickedKeyFile({required this.name, required this.text});

  final String name;
  final String text;
}

/// Where key files come from and go to. The core never touches the file system for a key the
/// person picks: the app reads the file here and hands the text over.
abstract interface class KeyFiles {
  /// Asks for a key file; null when the person cancelled.
  Future<PickedKeyFile?> pick();

  /// Asks where to save [text] as [fileName]; false when the person cancelled.
  Future<bool> save(String fileName, String text);
}

/// The system file dialogs, on Windows and Android.
class SystemKeyFiles implements KeyFiles {
  const SystemKeyFiles();

  @override
  Future<PickedKeyFile?> pick() async {
    final files = await FilePicker.pickFiles();
    final file = files.firstOrNull;
    if (file == null) return null;
    final bytes = await file.xFile.readAsBytes();
    return PickedKeyFile(name: file.name, text: utf8.decode(bytes, allowMalformed: true));
  }

  @override
  Future<bool> save(String fileName, String text) async {
    final saved = await FilePicker.saveFile(
      fileName: fileName,
      bytes: Uint8List.fromList(utf8.encode(text)),
    );
    return saved != null;
  }
}

/// For tests: returns [picked] and records what was saved.
class FakeKeyFiles implements KeyFiles {
  FakeKeyFiles({this.picked});

  PickedKeyFile? picked;
  final Map<String, String> saved = <String, String>{};

  @override
  Future<PickedKeyFile?> pick() async => picked;

  @override
  Future<bool> save(String fileName, String text) async {
    saved[fileName] = text;
    return true;
  }
}
