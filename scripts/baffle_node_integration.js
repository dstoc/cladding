const https = require("node:https");

https
  .get("https://localhost:8443/authorized/node", (response) => {
    let body = "";
    response.setEncoding("utf8");
    response.on("data", (chunk) => (body += chunk));
    response.on("end", () => {
      if (response.statusCode !== 200) {
        process.stderr.write(`unexpected Node.js response status: ${response.statusCode}\n`);
        process.exitCode = 1;
        return;
      }
      const result = JSON.parse(body);
      if (result.authorization !== "old") {
        process.stderr.write("Node.js request did not receive the authorized test credential\n");
        process.exitCode = 1;
      }
    });
  })
  .on("error", (error) => {
    process.stderr.write(`Node.js HTTPS proxy request failed: ${error.code || error.name}\n`);
    process.exitCode = 1;
  });
