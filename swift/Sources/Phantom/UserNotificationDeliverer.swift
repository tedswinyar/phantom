// The growth notification's delivery (v1.2 Phase 2, phantom-adq.2): the
// one place the app touches UserNotifications. Permission is requested
// lazily — the first time something is actually delivered — never at
// launch, so a user who never schedules a scan is never asked. Clicking the
// notification carries the scan id so the app can select it and open the
// History pane (wired with Phase 1's trigger).

import Foundation
import PhantomCore
import UserNotifications

final class UserNotificationDeliverer: GrowthAlertDelivering, @unchecked Sendable {
    static let scanIDKey = "phantomScanID"
    static let categoryIdentifier = "phantom.growth"

    private let center: UNUserNotificationCenter

    init(center: UNUserNotificationCenter = .current()) {
        self.center = center
    }

    func deliver(title: String, body: String, scanID: UUID) async {
        let settings = await center.notificationSettings()
        switch settings.authorizationStatus {
        case .notDetermined:
            let granted = (try? await center.requestAuthorization(options: [.alert, .sound])) ?? false
            guard granted else { return }
        case .denied:
            return
        default:
            break
        }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        content.categoryIdentifier = Self.categoryIdentifier
        content.userInfo = [Self.scanIDKey: scanID.uuidString.lowercased()]
        // One request per scan id: re-adding the same identifier replaces,
        // it never stacks — belt to the model's once-per-scan braces.
        let request = UNNotificationRequest(identifier: "\(Self.categoryIdentifier).\(scanID.uuidString.lowercased())", content: content, trigger: nil)
        try? await center.add(request)
    }
}
