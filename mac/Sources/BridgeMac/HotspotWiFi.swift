import CoreLocation
import CoreWLAN
import Foundation

/// Never include this type's fields in logs, errors or process arguments.
struct HotspotCredentials: CustomStringConvertible, CustomDebugStringConvertible {
    let ssid: String
    let password: String
    let security: String
    var description: String { "HotspotCredentials([redacted])" }
    var debugDescription: String { description }

    var isValid: Bool {
        guard !ssid.isEmpty, ssid.utf8.count <= 32 else { return false }
        switch security {
        case "open": return password.isEmpty
        case "wpa2", "wpa2-wpa3", "wpa3": return (8...63).contains(password.utf8.count)
        default: return false
        }
    }
}

/// Native CoreWLAN keeps passwords out of command lines and temporary files.
/// Modern macOS requires Location permission for Wi-Fi scans, even when SSID is already known.
final class HotspotWiFi: NSObject, CLLocationManagerDelegate {
    private let location = CLLocationManager()
    private var permissionReply: ((Bool) -> Void)?

    override init() {
        super.init()
        location.delegate = self
    }

    /// Call on the main thread before asking the phone to change its network state.
    func authorize(_ reply: @escaping (Bool) -> Void) {
        switch location.authorizationStatus {
        case .authorizedAlways, .authorizedWhenInUse: reply(true)
        case .notDetermined:
            permissionReply = reply
            location.requestWhenInUseAuthorization()
        default: reply(false)
        }
    }

    func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        guard manager.authorizationStatus != .notDetermined, let reply = permissionReply else { return }
        permissionReply = nil
        reply(manager.authorizationStatus == .authorizedAlways || manager.authorizationStatus == .authorizedWhenInUse)
    }

    /// Blocking; run off the UI thread. Error strings never include framework exception text.
    static func join(_ credentials: HotspotCredentials) -> String? {
        guard credentials.isValid else { return "Unsupported phone hotspot configuration." }
        guard let wifi = CWWiFiClient.shared().interface(), wifi.powerOn() else {
            return "Turn this Mac's Wi-Fi on and try again."
        }
        for _ in 0..<5 {
            do {
                let networks = try wifi.scanForNetworks(withSSID: Data(credentials.ssid.utf8))
                let network = networks.first { network in
                    guard network.ssidData == Data(credentials.ssid.utf8) else { return false }
                    switch credentials.security {
                    case "open": return network.supportsSecurity(.none)
                    case "wpa2", "wpa2-wpa3": return network.supportsSecurity(.wpa2Personal)
                    case "wpa3": return network.supportsSecurity(.wpa3Personal)
                    default: return false
                    }
                }
                if let network = network {
                    try wifi.associate(to: network, password: credentials.security == "open" ? nil : credentials.password)
                    return nil
                }
            } catch {
                // CoreWLAN exceptions can include configuration values; never forward them.
            }
            Thread.sleep(forTimeInterval: 2)
        }
        return "Could not join the phone hotspot. Check signal, security support and Bridge's Location permission in System Settings. The phone hotspot may still be on."
    }
}
