import { readFileSync } from "node:fs";

// Transitional support for existing static contracts. index.html loads /dk-app.js, which
// imports /receiver.js after the device key is unlocked; inline receiver.js at that tag.
export function readClientSource() {
  const html = readFileSync(new URL("../../public/index.html", import.meta.url), "utf8");
  return html.replace(/<script type="module" src="\/dk-app.js"><\/script>/,
    () => '<script type="module">\n' + readFileSync(new URL("../../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n") + '</script>');
}
