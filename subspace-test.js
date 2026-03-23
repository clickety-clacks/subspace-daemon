const { WebSocket } = require('ws');

const AGENT = 'l7Cgn7Ca0CFpxz7KQWI81ZfxtjMC6GurEFGy1Kq_C_Y';

const servers = [
  { name: 'subcom', url: 'ws://146.190.132.104/api/firehose/stream/websocket', token: '42a8759a2c5bfba3f9e56ec7646ceea281e7d5dd572e704dd0c2b34c44781157' },
  { name: 'subalt', url: 'ws://64.23.172.52/api/firehose/stream/websocket', token: 'b3e9a17f5c8d2e104a6f7b93d2c15e8a94d7f6b2e1c30a4589d27e6f1b8c5d40' },
];

let results = [];

function testServer({ name, url, token }) {
  return new Promise((resolve) => {
    const ws = new WebSocket(url);
    const result = { name, url, steps: [] };
    const timer = setTimeout(() => { result.steps.push('TIMEOUT'); ws.terminate(); resolve(result); }, 10000);

    ws.on('error', (e) => { result.steps.push('ERROR: ' + e.message); clearTimeout(timer); resolve(result); });
    ws.on('open', () => {
      result.steps.push('connected');
      ws.send(JSON.stringify({ topic: 'firehose', event: 'phx_join', payload: { agent_id: AGENT, session_token: token }, ref: '1' }));
    });
    ws.on('message', (buf) => {
      const msg = JSON.parse(buf.toString());
      if (msg.event === 'phx_reply' && msg.ref === '1') {
        if (msg.payload.status === 'ok') {
          result.steps.push('join_ok');
          ws.send(JSON.stringify({ topic: 'firehose', event: 'post_message', payload: { text: 'ufw-test-' + Date.now() }, ref: '2' }));
        } else {
          result.steps.push('join_failed: ' + JSON.stringify(msg.payload));
          clearTimeout(timer); ws.close(); resolve(result);
        }
      } else if (msg.event === 'phx_reply' && msg.ref === '2') {
        result.steps.push('post_reply: ' + msg.payload.status);
      } else if (msg.event === 'new_message') {
        result.steps.push('broadcast_received');
        clearTimeout(timer); ws.close(); resolve(result);
      } else if (msg.event === 'server_hello') {
        result.steps.push('server_hello: ' + msg.payload.server_name);
      }
    });
    ws.on('close', () => { if (!result.steps.includes('broadcast_received')) result.steps.push('closed'); });
  });
}

(async () => {
  for (const s of servers) {
    const r = await testServer(s);
    console.log(r.name + ': ' + r.steps.join(' → '));
  }
})();
