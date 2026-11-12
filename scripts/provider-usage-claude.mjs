// Metadata only. No message is yielded, so this cannot start an inference turn.
// Pinned SDK 0.3.321 exposes this experimental interface; unknown shapes fail closed.
import {pathToFileURL} from "node:url";
const {query}=await import(pathToFileURL(process.argv[2]).href);
const session=query({prompt:{async *[Symbol.asyncIterator](){await new Promise(()=>{});}},options:{
  cwd:process.cwd(),settingSources:[],mcpServers:{},tools:[],persistSession:false,
  pathToClaudeCodeExecutable:process.argv[3]??"claude",
  env:{...process.env,ANTHROPIC_API_KEY:undefined,ANTHROPIC_AUTH_TOKEN:undefined},
}});
try {
  const v=await session.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET({skipBehaviors:true});
  // Copy only the windows we consume; incidental grant data can be cached
  // without an independent observation timestamp and is not a qualified read.
  const keys=["five_hour","seven_day","seven_day_opus","seven_day_sonnet","seven_day_oauth_apps"];
  const limits=v.rate_limits && Object.fromEntries(keys.map(k=>[k,v.rate_limits[k] == null ? null : {
    utilization:v.rate_limits[k].utilization, resets_at:v.rate_limits[k].resets_at,
  }]));
  console.log(JSON.stringify({rate_limits_available:v.rate_limits_available,rate_limits:limits}));
} finally {session.close();}
process.exit(0);
