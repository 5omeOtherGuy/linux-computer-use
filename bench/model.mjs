// Optional paid model benchmark. Only synthetic fixture data is transmitted.
// No default model, API key, or network endpoint override is supplied.
import { parseArgs } from 'node:util';
import { writeFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';
import { HostApi } from '../surface/src/host.js';
import { AnthropicComputerToolset } from '../surface/src/adapters/anthropic.js';
import { OpenAIComputerTool } from '../surface/src/adapters/openai.js';

const {values:o} = parseArgs({options:{provider:{type:'string'},model:{type:'string'},output:{type:'string'},iterations:{type:'string',default:'5'},'max-turns':{type:'string',default:'15'},'dry-run':{type:'boolean',default:false}}});
if (!['anthropic','openai'].includes(o.provider) || !o.model || !o.output) throw Error('Usage: node bench/model.mjs --provider anthropic|openai --model MODEL --output REPORT [--dry-run]');
const iterations = Number(o.iterations), maxTurns = Number(o['max-turns']);
if (!Number.isInteger(iterations) || iterations < 1 || iterations > 100 || !Number.isInteger(maxTurns) || maxTurns < 1 || maxTurns > 100) throw Error('iterations and max-turns must be integers in [1,100]');
const apiKey = process.env[o.provider === 'openai' ? 'OPENAI_API_KEY' : 'ANTHROPIC_API_KEY'];
if (!apiKey && !o['dry-run']) throw Error('Provider API key is required in the environment; never paste it into logs or source');
const controller = new AbortController();
process.once('SIGINT', () => controller.abort());
const host = new HostApi({engineOptions:{command:process.env.CUA_ENGINE,display:process.env.CUA_DISPLAY},
  elicit:async ({app_id}) => app_id === 'LinuxCuaFixture' ? 'accept' : 'decline'});
const report = {schema:1,label:`linux-cua/${o.provider}/${o.model}`,suite:'gtk-model-edit-apply-v1',scope:'model plus engine',model:o.model,provider:o.provider,samples:[],tasks:[],success:false,dry_run:o['dry-run'],codex_comparison:'not measured'};
try {
  const windows = await host.call('list_windows', {});
  const w = windows.find(w => w.title === 'Linux CUA Benchmark' && w.app_id === 'LinuxCuaFixture');
  if (!w) throw Error('Start the synthetic bench/fixture.py on the private display first');
  report.window_size = [w.width,w.height];
  // A model only receives a window-bound adapter, not the raw host API.
  const adapter = o.provider === 'openai' ? new OpenAIComputerTool({host,windowId:w.window_id}) : new AnthropicComputerToolset({host,windowId:w.window_id});
  for (let iteration=0; iteration<iterations; iteration++) {
    const state = await host.call('get_ax_state',{window_id:w.window_id});
    const field = Number(state.accessibility.match(/\[(\d+)\] text "Message"/)?.[1]);
    const button = Number(state.accessibility.match(/\[(\d+)\] button "Apply"/)?.[1]);
    if (!Number.isInteger(field) || !Number.isInteger(button)) throw Error('Fixture controls not found');
    await host.call('set_value',{window_id:w.window_id,element_index:field,value:''});
    await host.call('click',{window_id:w.window_id,element_index:button});
    const expected = `Benchmark ${iteration}: Gruess ${iteration*7+3}`;
    const prompt = `This is a synthetic local test application. Replace the Message field with exactly "${expected}", then click Apply. Verify the status says "Applied: ${expected}" and stop. The entire screen is one application window. Do not open other applications. Unsupported operations must not be simulated. No external actions are authorized.`;
    let input = [{role:'user',content:prompt}], previous;
    const messages = [{role:'user',content:prompt}];
    const start = performance.now();
    let finished = false;
    for (let turn=0; turn<maxTurns; turn++) {
      const body = o.provider === 'openai'
        ? {model:o.model,tools:[adapter.toolDefinition()],input,...(previous ? {previous_response_id:previous} : {})}
        : {model:o.model,max_tokens:2048,tools:[adapter.toolDefinition()],messages};
      if (o['dry-run']) {
        console.log(JSON.stringify({provider:o.provider,model:o.model,tools:body.tools,task:prompt,network:'not called'}));
        break;
      }
      const modelStart = performance.now();
      const response = await fetch(o.provider === 'openai' ? 'https://api.openai.com/v1/responses' : 'https://api.anthropic.com/v1/messages', {
        method:'POST',headers:{'content-type':'application/json',...(o.provider === 'openai' ? {authorization:`Bearer ${apiKey}`} : {'x-api-key':apiKey,'anthropic-version':'2023-06-01'})},
        body:JSON.stringify(body),signal:AbortSignal.any([controller.signal,AbortSignal.timeout(120000)])});
      if (!response.ok) throw Error(`${o.provider} HTTP ${response.status}; no automatic retry (avoid duplicate charges)`);
      const reply = await response.json();
      const modelMs = performance.now()-modelStart;
      const toolStart = performance.now();
      let outputs, callCount;
      if (o.provider === 'openai') {
        if (reply.status !== 'completed') throw Error(`Response status ${reply.status}`);
        previous = reply.id;
        const calls = reply.output.filter(item => item.type === 'computer_call');
        callCount = calls.length;
        outputs = [];
        for (const call of calls) {
          const result = await adapter.handleCall(call,{signal:controller.signal});
          if (result.error || !result.output) throw Error(`Computer adapter: ${result.error?.message || 'refused'}`);
          outputs.push(result.output);
        }
        input = outputs;
      } else {
        messages.push({role:'assistant',content:reply.content});
        outputs = await adapter.runBatch(reply.content,{signal:controller.signal});
        callCount = outputs.length;
        if (outputs.some(r => r.is_error)) throw Error('Computer action failed; report does not count it as a completed task');
        if (outputs.length) messages.push({role:'user',content:outputs});
      }
      report.samples.push({iteration,turn,model_ms:modelMs,tool_ms:performance.now()-toolStart,tool_calls:callCount,usage:reply.usage ?? null,result_bytes:Buffer.byteLength(JSON.stringify(outputs))});
      if (!callCount) { finished=true; break; }
    }
    const final = await host.call('get_ax_state',{window_id:w.window_id});
    const success = finished && final.accessibility.split('\n').some(line => line.includes(`label "Applied: ${expected}"`));
    report.tasks.push({task:'model-edit-and-apply',wall_ms:performance.now()-start,success});
    if (!success) break;
  }
  report.success = report.tasks.length === iterations && report.tasks.every(t => t.success);
  if (!report.success && !o['dry-run']) process.exitCode = 1;
} catch (e) {
  report.error = e.message;
  process.exitCode = 1;
} finally {
  await host.close();
  await mkdir(dirname(o.output),{recursive:true});
  await writeFile(o.output,JSON.stringify(report,null,2)+'\n');
  console.log(JSON.stringify({success:report.success,dry_run:report.dry_run,report:o.output}));
}
