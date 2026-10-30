# Fonts

Atkinson Hyperlegible Next, Fragment Mono and Bricolage Grotesque are bundled
under the SIL Open Font License 1.1; retain the notices in `licenses/`.
The gallery registers these families in its pubspec and loads the same files
in `test/flutter_test_config.dart`, so golden screenshots draw real text.
That test setup also registers every Lucide font family from the resolved
`lucide_icons_flutter` package under its package-qualified name, so icons render
as glyphs instead of missing-font boxes. Missing icon fonts fail test setup.
