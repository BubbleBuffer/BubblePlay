import { readFileSync } from "node:fs";

// Transitional support for existing static contracts. Follow the real entrypoint.
export function readClientSource() {
  const html = readFileSync(new URL("../../public/index.html", import.meta.url), "utf8");
  return html.replace(/<script type="module" src="\/receiver.js"><\/script>/,
    () => '<script type="module">\n' + readFileSync(new URL("../../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n") + '</script>');
}
