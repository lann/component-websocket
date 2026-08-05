// The `lann:component-test/test-context` shim (runner-is-provider
// topology): the harness constructs one per case and collects the
// diagnostics sideband into the case's results event.
export class Context {
  constructor(sink) {
    this.sink = sink;
  }
  async diagnostic(msg) {
    this.sink.push(msg);
  }
}
