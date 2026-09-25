// swift-tools-version:5.9
//
// ApexStudio — native macOS frontend for the Apex-AOSP VMM.
// Build the Rust core first: `cargo build --release -p apex-vmm`
// (scripts/build-macos.sh does both and signs the result).

import Foundation
import PackageDescription

let packageDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let rustLibDir = packageDir.appendingPathComponent("../../target/release").standardizedFileURL.path

let package = Package(
    name: "ApexStudio",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "ApexStudio", targets: ["ApexStudio"])],
    targets: [
        .target(
            name: "CApex",
            path: "Sources/CApex",
            publicHeadersPath: "include",
            linkerSettings: [
                .unsafeFlags(["-L", rustLibDir]),
                .linkedLibrary("apex_vmm"),
                .linkedFramework("Hypervisor"),
            ]
        ),
        .executableTarget(
            name: "ApexStudio",
            dependencies: ["CApex"],
            path: "Sources/ApexStudio",
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("Metal"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("IOKit"),
            ]
        ),
    ]
)
