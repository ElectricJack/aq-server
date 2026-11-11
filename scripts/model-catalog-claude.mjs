// Metadata only: the input stream never emits a prompt.
import { pathToFileURL } from "node:url";
const { query } = await import(pathToFileURL(process.argv[2]).href);
const input = { async *[Symbol.asyncIterator]() { await new Promise(() => {}); } };
const session = query({ prompt: input, options: {
  cwd: process.cwd(), settingSources: [], mcpServers: {}, tools: [],
  persistSession: false, pathToClaudeCodeExecutable: process.argv[3] ?? "claude",
  extraArgs: { bare: null },
  env: { ...process.env, ANTHROPIC_API_KEY: undefined, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1" },
} });
try {
  console.log(JSON.stringify(await session.supportedModels()));
} finally {
  session.close();
}
process.exit(0);
