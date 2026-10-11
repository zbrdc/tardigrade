vm.fs.write("/greeting.txt", "Hello, RAM");
vm.fs.write("/note.txt", "everything lives in memory");
console.log(vm.fs.read("/greeting.txt"));
console.log("files: " + vm.fs.list("/").join(","));
vm.fs.remove("/note.txt");
console.log("after remove: " + vm.fs.list("/").join(","));
