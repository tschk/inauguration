const std = @import("std");
pub fn build(b: *std.Build) void {
    const target = b.resolveTargetQuery(.{ .cpu_arch = .thumb, .os_tag = .freestanding,
        .abi = .eabi, .cpu_model = .{ .explicit = &std.Target.arm.cpu.cortex_m33 } });
    const module = b.createModule(.{ .target = target, .optimize = .ReleaseSmall });
    module.addObjectFile(b.path("values.o"));
    module.addAssemblyFile(b.path("startup.S"));
    const elf = b.addExecutable(.{ .name = "stack-args", .root_module = module });
    elf.entry = .{ .symbol_name = "Reset_Handler" };
    elf.link_gc_sections = true;
    elf.linker_script = b.path("memory.ld");
    b.installArtifact(elf);
}
