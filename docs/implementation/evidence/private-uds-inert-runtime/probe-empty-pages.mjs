import { chromium, expect } from "/tmp/aq-dashboard-actions-ui-v2/node_modules/@playwright/test/index.mjs";
import { mkdir, writeFile } from "node:fs/promises";
const origin="http://127.0.0.1:4294", out="/tmp/aq-private-uds-owner-browser";
const owner="97ecbd79-e439-4b3a-a1b3-4337a166253f", digest="sha256:5c0fcfa4fe11cd95a9081edeb7323ef07f9d2c66bcf24b2add1ef546200ac667";
await mkdir(out,{recursive:true}); const browser=await chromium.launch({headless:true});
const errors=[],mutations=[],foreign=[],responses=[]; const started=Date.now();
try {
 const context=await browser.newContext();const page=await context.newPage();
 page.on("pageerror",e=>errors.push(e.message));page.on("request",r=>{if(!["GET","HEAD"].includes(r.method()))mutations.push(r.method()+" "+r.url());if(new URL(r.url()).origin!==origin)foreign.push(r.url());});
 page.on("response",r=>{if(r.url().includes("/api/"))responses.push({path:new URL(r.url()).pathname,status:r.status()});});
 const bootstrap=await(await context.request.get(origin+"/api/v1/bootstrap")).json();expect(bootstrap.data.principal.id).toBe(owner);expect(bootstrap.data.product.dashboard_digest).toBe(digest);
 for(const cap of ["decisions.read","playbooks.inspect","playbooks.loaded-definitions.read"])expect(bootstrap.data.capabilities.find(c=>c.name===cap)).toMatchObject({enabled:true,version:"1"});
 for(const width of [1440,390,320]){
  await page.setViewportSize({width,height:width===1440?1100:844});
  for(const [path,heading,text,name]of[["/playbooks","Recent runs","No runs recorded in this configured namespace.","runs"],["/decisions","Decisions","No available reviews in this page.","decisions"]]){
   await page.goto(origin+path);await expect(page.getByRole("heading",{name:heading,exact:true})).toBeVisible();await expect(page.getByText(text,{exact:true})).toBeVisible();
   expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
   await page.screenshot({path:out+"/"+name+"-"+width+".png",fullPage:width===1440});
  }
 }
 expect(errors).toEqual([]);expect(mutations).toEqual([]);expect(foreign).toEqual([]);expect(responses.filter(r=>r.status>=400)).toEqual([]);
 await writeFile(out+"/empty-pages-result.json",JSON.stringify({passed:true,elapsed_ms:Date.now()-started,origin,principal:owner,bundle_digest:digest,viewports:[1440,390,320],mutations,foreign,errors,responses,browser_interception:false,scope:"Fresh isolated real UDS Owner; unavailable structural project context; no eligibility or workflow execution claim"},null,2)+"\n");
}finally{await browser.close();}
