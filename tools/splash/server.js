// Serves the splash scene and writes the frames it posts: node server.js <three dir> <port>
const http = require("http");
const fs = require("fs");
const path = require("path");
const here = __dirname;
const threeDir = process.argv[2];
const port = Number(process.argv[3] || 8765);
const types = { ".html": "text/html", ".js": "text/javascript", ".woff2": "font/woff2" };
fs.mkdirSync(path.join(here, "frames"), { recursive: true });
http
  .createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    if (req.method === "POST") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        if (url.pathname === "/frame") {
          const i = String(url.searchParams.get("i")).padStart(3, "0");
          fs.writeFileSync(path.join(here, "frames", `f${i}.png`), Buffer.from(body.split(",")[1], "base64"));
        } else if (url.pathname === "/done") {
          console.log("done");
          setTimeout(() => process.exit(0), 100);
        }
        res.end("ok");
      });
      return;
    }
    let file;
    if (url.pathname === "/" || url.pathname === "/index.html") file = path.join(here, "index.html");
    else if (url.pathname === "/bricolage.woff2") file = path.join(here, "bricolage.woff2");
    else if (url.pathname.startsWith("/three/")) file = path.join(threeDir, url.pathname.slice(7));
    if (!file || !file.startsWith(here) && !file.startsWith(threeDir) || !fs.existsSync(file)) {
      res.statusCode = 404;
      return res.end();
    }
    res.setHeader("Content-Type", types[path.extname(file)] || "application/octet-stream");
    fs.createReadStream(file).pipe(res);
  })
  .listen(port, "127.0.0.1", () => console.log("listening", port));
