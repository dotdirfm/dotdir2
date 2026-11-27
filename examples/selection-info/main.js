// Executed by QuickJS on the extension worker. UI data is rendered natively.
let refreshes = 0;
dotdir.registerCommand('example.selection.summary', 'Example: Show Selection Summary', context => ({
  message: `${context.selected.length} item(s) selected in ${context.path}`
}));
dotdir.registerPanel('example.selection.inspect', 'Example: Selection Inspector', (context, event) => {
  if (event?.id === 'refresh') refreshes++;
  return {panel: {type: 'column', children: [
    {type: 'text', text: 'Selection Inspector'},
    {type: 'text', text: context.path, muted: true},
    {type: 'table', columns: ['Name', 'Kind', 'Size'], rows: context.selected.map(e => [e.name, e.directory ? 'Directory' : 'File', String(e.size)])},
    {type: 'button', id: 'refresh', text: 'Refresh selection'},
    {type: 'text', text: `QuickJS · native GPUI controls · ${refreshes} refreshes`, muted: true}
  ]}};
});
