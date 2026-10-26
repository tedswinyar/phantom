// The inspector's width preference and the handle that drags it. Pure
// SwiftUI on purpose: the resizable inspector used to come from an
// HSplitView, and that split view wrapped around the detail's VSplitView is
// what looped and crashed the app on macOS 27 (phantom-xb1, GitHub #1);
// `.inspector(isPresented:)` looped the same way. Nothing here is an AppKit
// split container or reports a minimum size to AppKit, so the loop has no
// way back in through this view. The width band and the handle's grab area
// are DesignKit tokens because both are terms of the window floor.

import SwiftUI
import DesignKit

/// The inspector width preference, clamped to the DesignKit band on every
/// read so a stale or hand-edited default can never widen the inspector past
/// what `Size.windowMinWidth` accounts for.
enum InspectorWidth {
    static let storageKey = "inspectorWidth"

    static func clamped(_ proposed: Double) -> CGFloat {
        guard proposed.isFinite else { return Size.inspectorDefaultWidth }
        return min(max(CGFloat(proposed), Size.inspectorMinWidth), Size.inspectorMaxWidth)
    }
}

/// A separator line in a `Size.resizeHandleWidth` grab area between the
/// detail and the inspector. The inspector is to the RIGHT, so dragging the
/// handle left widens it. Keyboard and VoiceOver users adjust it as a value.
struct InspectorResizeHandle: View {
    @Binding var width: Double
    @State private var widthAtDragStart: Double?

    private let keyboardStep: Double = 10

    var body: some View {
        ZStack {
            Color.clear
            Rectangle()
                .fill(Palette.separator)
                .frame(width: Size.dividerWidth)
        }
        .frame(width: Size.resizeHandleWidth)
        .contentShape(Rectangle())
        .onHover { inside in
            if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() }
        }
        .gesture(
            DragGesture(minimumDistance: 1, coordinateSpace: .global)
                .onChanged { value in
                    let start = widthAtDragStart ?? width
                    widthAtDragStart = start
                    width = Double(InspectorWidth.clamped(start - value.translation.width))
                }
                .onEnded { _ in widthAtDragStart = nil }
        )
        .help("Drag to resize the \(Vocabulary.inspector.lowercased())")
        .accessibilityElement()
        .accessibilityLabel("\(Vocabulary.inspector) width")
        .accessibilityValue("\(Int(InspectorWidth.clamped(width))) points")
        .accessibilityAdjustableAction { direction in
            switch direction {
            case .increment: width = Double(InspectorWidth.clamped(width + keyboardStep))
            case .decrement: width = Double(InspectorWidth.clamped(width - keyboardStep))
            @unknown default: break
            }
        }
    }
}
