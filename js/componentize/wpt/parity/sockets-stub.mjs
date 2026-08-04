// The browser-profile map target for `wasi:sockets/*`: the preview2-shim
// browser build predates resource classes (no TcpSocket/UdpSocket/
// ResolveAddressStream/Network exports), and the generated bindings
// hard-fail on any undefined import at instantiation. The componentize-js
// runtime imports wasi:sockets unconditionally, but the parity runner
// never opens a socket, so every member here exists only to satisfy the
// import check — and throws if actually reached, which would mean the
// runner grew a socket dependency this stub is silently breaking.

const unreachable = (what) => {
  throw new Error(`wasi:sockets is stubbed in the browser profile: ${what} was called`);
};

class StubResource {
  constructor() {
    unreachable(new.target.name);
  }
}

export class ResolveAddressStream extends StubResource {}
export class Network extends StubResource {}
export class TcpSocket extends StubResource {}
export class UdpSocket extends StubResource {}
export class IncomingDatagramStream extends StubResource {}
export class OutgoingDatagramStream extends StubResource {}

export const instanceNetwork = {
  instanceNetwork: () => unreachable("instance-network.instance-network"),
};
export const ipNameLookup = {
  ResolveAddressStream,
  resolveAddresses: () => unreachable("ip-name-lookup.resolve-addresses"),
};
export const network = { Network };
export const tcp = { TcpSocket };
export const tcpCreateSocket = {
  createTcpSocket: () => unreachable("tcp-create-socket.create-tcp-socket"),
};
export const udp = { UdpSocket, IncomingDatagramStream, OutgoingDatagramStream };
export const udpCreateSocket = {
  createUdpSocket: () => unreachable("udp-create-socket.create-udp-socket"),
};
