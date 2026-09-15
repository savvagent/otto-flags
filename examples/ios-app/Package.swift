// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "OttoFlagsExample",
    platforms: [
        .iOS(.v17)
    ],
    products: [
        .library(
            name: "OttoFlagsExample",
            targets: ["OttoFlagsExample"]),
    ],
    dependencies: [
        .package(path: "../../packages/ios-sdk")
    ],
    targets: [
        .target(
            name: "OttoFlagsExample",
            dependencies: [
                .product(name: "OttoFlagsSDK", package: "ios-sdk")
            ]
        )
    ]
)
