import http from "node:http";
import process from "node:process";

const args = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  args.set(process.argv[index], process.argv[index + 1]);
}

const host = args.get("--host");
const port = Number(args.get("--port"));
if (host !== "127.0.0.1" || !Number.isInteger(port)) {
  throw new Error("mock-dsh 只允许显式回环 host 和整数 port");
}

const server = http.createServer((_request, response) => {
  response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
  response.end("<!doctype html><title>Mock DSH</title><h1>Mock DSH Ready</h1>");
});

server.listen(port, host, () => {
  console.log(`READY http://${host}:${port}`);
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    server.close(() => process.exit(0));
  });
}
