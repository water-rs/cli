import Foundation

/// Resources shipped with the generated WaterUI package.
public enum WaterUIResources {
    public static var assets: URL {
        guard let url = Bundle.module.url(forResource: "waterui_assets", withExtension: nil) else {
            fatalError("The WaterUI package is missing its asset bundle")
        }
        return url
    }

    public static var fonts: URL {
        guard let url = Bundle.module.url(forResource: "fonts", withExtension: nil) else {
            fatalError("The WaterUI package is missing its font bundle")
        }
        return url
    }
}
