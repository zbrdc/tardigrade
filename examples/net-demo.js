// In-memory network: ports, listeners, replies across loop turns.
const server = vm.net.port();
vm.net.listen(server, (payload, from) => {
  console.log("server got: " + payload);
  vm.net.send(server, from, "echo:" + payload);
});

const me = vm.net.port();
vm.net.listen(me, (payload) => console.log("client got: " + payload));
vm.net.send(me, server, "ping");

setTimeout(() => vm.exit(0), 20);
