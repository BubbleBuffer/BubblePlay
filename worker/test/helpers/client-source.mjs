import { readFileSync } from "node:fs";

// Transitional support for existing static contracts. index.html loads /dk-app.js, which
// imports /receiver.js after the device key is unlocked; inline receiver.js at that tag.
// The bootstrap script is inlined the same way, so the contracts see one page source.
const read = name => readFileSync(new URL(`../../public/${name}`, import.meta.url), "utf8").replace(/\r\n/g, "\n");

export function readClientSource() {
  return readFileSync(new URL("../../public/index.html", import.meta.url), "utf8")
    .replace(/<script src="\/dk-bootstrap.js"><\/script>/, () => "<script>\n" + read("dk-bootstrap.js") + "</script>")
    .replace(/<script type="module" src="\/dk-app.js"><\/script>/, () => '<script type="module">\n' + read("receiver.js") + "</script>");
}
