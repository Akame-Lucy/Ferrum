# Ferrite Plugin System

Ferrite provides a real-time event streaming WebSocket API allowing external processes, formatters, linters, and audit tools to hook into system events without modifying core server source code.

## Event Stream Endpoint

Connect via WebSocket to:

```
ws://<ferrite-host>:<port>/ws/events
```

## Supported Event Schemas

All event payloads sent over `/ws/events` are JSON objects formatted with a `type` discriminator and a `data` object payload.

### 1. File Opened Event
Triggered whenever a user opens a remote file in the editor pane.

```json
{
  "type": "FileOpened",
  "data": {
    "remote": "production-sftp",
    "path": "/var/www/index.js"
  }
}
```

### 2. File Saved Event
Triggered whenever a user saves changes to a remote file.

```json
{
  "type": "FileSaved",
  "data": {
    "remote": "production-sftp",
    "path": "/var/www/index.js"
  }
}
```

### 3. File Closed Event (defined, not yet emitted)

`FileClosed` exists in the event schema below but nothing in Ferrite sends it
yet (tab close is currently client-side only, with no server round trip).
Don't build a plugin that depends on it firing until this note is removed.

```json
{
  "type": "FileClosed",
  "data": {
    "remote": "production-sftp",
    "path": "/var/www/index.js"
  }
}
```

## Example Node.js Plugin Script

```javascript
const WebSocket = require('ws');

const ws = new WebSocket('ws://localhost:8080/ws/events');

ws.on('open', () => {
    console.log('Connected to Ferrite Event Stream');
});

ws.on('message', (data) => {
    const event = JSON.parse(data);
    console.log(`Received Event [${event.type}]:`, event.data);

    if (event.type === 'FileSaved') {
        console.log(`Triggering automated linter/formatter on ${event.data.path}...`);
    }
});
```
