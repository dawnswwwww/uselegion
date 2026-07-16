//! Minimal Web Dashboard static assets for the Legion Gateway.
//!
//! The dashboard is served from `/dashboard` and provides a simple chat UI
//! that connects to the Gateway WebSocket endpoint at `/ws`.

use axum::Router;
use axum::body::Body;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

/// Returns the single-page dashboard HTML.
pub async fn dashboard_html() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Returns the dashboard JavaScript asset with the correct `text/javascript` MIME type.
pub async fn dashboard_js() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            mime::APPLICATION_JAVASCRIPT.as_ref(),
        )],
        Body::from(DASHBOARD_JS),
    )
        .into_response()
}

/// Builds an axum router that serves the dashboard and its static assets.
pub fn router() -> Router {
    Router::new()
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/assets/dashboard.js", get(dashboard_js))
}

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Legion Dashboard</title>
    <style>
        :root { --bg: #0f1115; --panel: #181b21; --text: #e6e6e6; --muted: #8b949e; --accent: #58a6ff; --border: #30363d; }
        * { box-sizing: border-box; }
        body { margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: var(--bg); color: var(--text); height: 100vh; display: flex; flex-direction: column; }
        header { padding: 1rem 1.5rem; border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
        header h1 { margin: 0; font-size: 1.25rem; }
        #status { font-size: 0.875rem; color: var(--muted); }
        #status.connected { color: #3fb950; }
        main { flex: 1; display: flex; overflow: hidden; }
        #sidebar { width: 260px; border-right: 1px solid var(--border); padding: 1rem; background: var(--panel); }
        #sidebar h2 { font-size: 0.875rem; text-transform: uppercase; color: var(--muted); margin: 0 0 0.75rem; }
        #log { list-style: none; margin: 0; padding: 0; font-size: 0.8125rem; color: var(--muted); max-height: 100%; overflow-y: auto; }
        #log li { margin-bottom: 0.5rem; word-break: break-word; }
        #chat { flex: 1; display: flex; flex-direction: column; padding: 1rem; }
        #messages { flex: 1; overflow-y: auto; display: flex; flex-direction: column; gap: 0.75rem; }
        .message { max-width: 80%; padding: 0.75rem 1rem; border-radius: 0.75rem; line-height: 1.45; white-space: pre-wrap; }
        .message.user { align-self: flex-end; background: var(--accent); color: #fff; }
        .message.assistant { align-self: flex-start; background: var(--panel); border: 1px solid var(--border); }
        .message.tool { align-self: flex-start; background: #21262d; border: 1px dashed var(--border); font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.8125rem; }
        .message .label { font-size: 0.75rem; color: var(--muted); margin-bottom: 0.25rem; text-transform: uppercase; }
        #input-area { display: flex; gap: 0.5rem; margin-top: 1rem; }
        #message-input { flex: 1; padding: 0.75rem 1rem; border-radius: 0.5rem; border: 1px solid var(--border); background: var(--panel); color: var(--text); }
        button { padding: 0.75rem 1.25rem; border-radius: 0.5rem; border: none; background: var(--accent); color: #fff; cursor: pointer; }
        button:disabled { opacity: 0.6; cursor: not-allowed; }
    </style>
</head>
<body>
    <header>
        <h1>Legion Dashboard</h1>
        <div id="status">Disconnected</div>
    </header>
    <main>
        <aside id="sidebar">
            <h2>Events</h2>
            <ul id="log"></ul>
        </aside>
        <section id="chat">
            <div id="messages"></div>
            <form id="input-area">
                <input id="message-input" type="text" placeholder="Send a message to the agent..." autocomplete="off" />
                <button type="submit" id="send-btn">Send</button>
            </form>
        </section>
    </main>
    <script src="/dashboard/assets/dashboard.js"></script>
</body>
</html>"#;

const DASHBOARD_JS: &str = r#"(function () {
    const statusEl = document.getElementById('status');
    const logEl = document.getElementById('log');
    const messagesEl = document.getElementById('messages');
    const inputEl = document.getElementById('message-input');
    const formEl = document.getElementById('input-area');
    const sendBtn = document.getElementById('send-btn');

    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}/ws`;
    const ws = new WebSocket(wsUrl);
    let reqCounter = 0;
    let currentAssistantMessage = null;

    function log(text) {
        const li = document.createElement('li');
        li.textContent = `${new Date().toLocaleTimeString()} ${text}`;
        logEl.appendChild(li);
        logEl.scrollTop = logEl.scrollHeight;
    }

    function appendMessage(role, html) {
        const div = document.createElement('div');
        div.className = `message ${role}`;
        div.innerHTML = `<div class="label">${role}</div><div class="content">${html}</div>`;
        messagesEl.appendChild(div);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return div;
    }

    function setConnected(connected) {
        statusEl.textContent = connected ? 'Connected' : 'Disconnected';
        statusEl.classList.toggle('connected', connected);
        sendBtn.disabled = !connected;
    }

    ws.onopen = () => {
        log('WebSocket open');
        ws.send(JSON.stringify({
            type: 'connect',
            id: 'conn-dashboard',
            params: {
                auth: { token: '' },
                deviceId: 'web-dashboard',
                platform: 'web',
                deviceFamily: 'client',
                role: 'client'
            }
        }));
    };

    ws.onclose = () => {
        log('WebSocket closed');
        setConnected(false);
    };

    ws.onerror = (err) => {
        log('WebSocket error');
        console.error(err);
        setConnected(false);
    };

    ws.onmessage = (event) => {
        let frame;
        try {
            frame = JSON.parse(event.data);
        } catch (e) {
            log('Invalid JSON from gateway');
            return;
        }

        if (frame.type === 'res' && frame.id === 'conn-dashboard') {
            if (frame.ok) {
                setConnected(true);
                log(`Hello from ${frame.payload.gateway_id}`);
            } else {
                log(`Connect failed: ${frame.error || 'unknown'}`);
            }
            return;
        }

        if (frame.type === 'res') {
            log(`Response ${frame.id}: ${frame.ok ? 'ok' : frame.error}`);
            return;
        }

        if (frame.type === 'event' && frame.event === 'agent') {
            const p = frame.payload || {};
            if (p.stream === 'lifecycle') {
                log(`Run ${p.run_id} ${p.phase}`);
                if (p.phase === 'start') {
                    currentAssistantMessage = appendMessage('assistant', '');
                }
            } else if (p.stream === 'assistant') {
                if (!currentAssistantMessage) {
                    currentAssistantMessage = appendMessage('assistant', '');
                }
                const content = currentAssistantMessage.querySelector('.content');
                content.textContent += p.delta || '';
                messagesEl.scrollTop = messagesEl.scrollHeight;
            } else if (p.stream === 'tool') {
                const state = p.state || 'update';
                const tool = p.tool_call || {};
                const text = `[${state}] ${tool.name || 'tool'} ${JSON.stringify(tool.arguments || {})}`;
                appendMessage('tool', text);
                log(`Tool ${state}: ${tool.name || 'unknown'}`);
            } else if (p.stream === 'compaction') {
                log(`Compaction: ${p.summary}`);
            }
            return;
        }

        log(`Event ${frame.event || frame.type}`);
    };

    formEl.addEventListener('submit', (e) => {
        e.preventDefault();
        const text = inputEl.value.trim();
        if (!text) return;

        appendMessage('user', text);
        inputEl.value = '';
        currentAssistantMessage = null;

        const id = `req-${++reqCounter}`;
        ws.send(JSON.stringify({
            type: 'req',
            id,
            method: 'agent',
            params: {
                sessionKey: 'agent:main:dm:webchat:default:direct:dashboard',
                message: { role: 'user', content: text },
                idempotencyKey: id,
                wait: true
            }
        }));
        log(`Sent agent request ${id}`);
    });
})();"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_contain_dashboard_html() {
        assert!(DASHBOARD_HTML.contains("Legion Dashboard"));
        assert!(DASHBOARD_HTML.contains("/dashboard/assets/dashboard.js"));
    }

    #[test]
    fn should_contain_websocket_logic() {
        assert!(DASHBOARD_JS.contains("WebSocket"));
        assert!(DASHBOARD_JS.contains("agent"));
    }
}
