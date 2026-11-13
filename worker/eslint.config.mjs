import js from "@eslint/js";
import globals from "globals";

// Thresholds are shared with the other gates: 500 lines per file, about 80 lines per function.
const MAX_FILE_LINES = 500;
const MAX_FUNCTION_LINES = 80;
const ALLOWED_NUMBERS = [0, 1, -1];

export default [
  { ignores: ["public/vendor/**", "node_modules/**", ".wrangler/**"] },
  js.configs.recommended,
  {
    files: ["public/**/*.js"],
    languageOptions: { sourceType: "module", globals: { ...globals.browser, ...globals.worker } },
  },
  {
    files: ["test/**/*.mjs", "test-e2e/**/*.mjs", "*.mjs"],
    languageOptions: { sourceType: "module", globals: { ...globals.node } },
  },
  {
    rules: {
      "no-empty": "error",
      "no-unused-vars": "error",
      "max-lines": ["error", { max: MAX_FILE_LINES }],
      "max-lines-per-function": ["error", { max: MAX_FUNCTION_LINES, skipBlankLines: true, skipComments: true }],
    },
  },
  {
    // Tests may use bare numbers; product code names them.
    files: ["public/**/*.js"],
    rules: { "no-magic-numbers": ["error", { ignore: ALLOWED_NUMBERS, ignoreArrayIndexes: true }] },
  },
];
