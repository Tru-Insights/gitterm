#!/usr/bin/env swift

import AppKit
import Foundation

// Create a 1024x1024 icon (macOS standard) with proper padding
let size = NSSize(width: 1024, height: 1024)
let image = NSImage(size: size)

image.lockFocus()

let context = NSGraphicsContext.current!.cgContext

// macOS icon grid: content should be ~824x824 centered in 1024x1024
// With ~100px padding on each side
let padding: CGFloat = 100
let contentSize: CGFloat = 824
let cornerRadius: CGFloat = 185  // macOS squircle corner radius (~22.5% of size)

// Draw shadow first (offset down and slightly larger)
let shadowRect = NSRect(x: padding - 4, y: padding - 20, width: contentSize + 8, height: contentSize + 8)
let shadowPath = NSBezierPath(roundedRect: shadowRect, xRadius: cornerRadius + 2, yRadius: cornerRadius + 2)
NSColor(white: 0, alpha: 0.3).setFill()
shadowPath.fill()

// Blur the shadow (draw multiple times with decreasing alpha)
for i in 1...8 {
    let offset = CGFloat(i) * 3
    let alpha = 0.15 / CGFloat(i)
    let blurRect = NSRect(x: padding - offset, y: padding - 20 - offset, width: contentSize + offset * 2, height: contentSize + offset * 2)
    let blurPath = NSBezierPath(roundedRect: blurRect, xRadius: cornerRadius + offset, yRadius: cornerRadius + offset)
    NSColor(white: 0, alpha: alpha).setFill()
    blurPath.fill()
}

// Main icon background with squircle shape
let iconRect = NSRect(x: padding, y: padding, width: contentSize, height: contentSize)
let iconPath = NSBezierPath(roundedRect: iconRect, xRadius: cornerRadius, yRadius: cornerRadius)

// Clip to squircle for all content
context.saveGState()
iconPath.addClip()

// Gradient background (dark terminal)
let gradient = NSGradient(colors: [
    NSColor(red: 0.15, green: 0.15, blue: 0.18, alpha: 1.0),  // Dark gray top
    NSColor(red: 0.10, green: 0.10, blue: 0.12, alpha: 1.0)   // Darker bottom
])!
gradient.draw(in: iconRect, angle: -90)

// Subtle inner glow/highlight at top
let highlightRect = NSRect(x: padding, y: padding + contentSize - 200, width: contentSize, height: 200)
let highlightGradient = NSGradient(colors: [
    NSColor(white: 1.0, alpha: 0.08),
    NSColor(white: 1.0, alpha: 0.0)
])!
highlightGradient.draw(in: highlightRect, angle: -90)

// Title bar area (slightly lighter)
let titleBarHeight: CGFloat = 140
let titleBarRect = NSRect(x: padding, y: padding + contentSize - titleBarHeight, width: contentSize, height: titleBarHeight)
NSColor(red: 0.20, green: 0.20, blue: 0.23, alpha: 1.0).setFill()
NSBezierPath(rect: titleBarRect).fill()

// Divider line under title bar
NSColor(red: 0.12, green: 0.12, blue: 0.14, alpha: 1.0).setFill()
NSBezierPath(rect: NSRect(x: padding, y: padding + contentSize - titleBarHeight - 2, width: contentSize, height: 2)).fill()

// Window buttons (red, yellow, green) - macOS style
let buttonY = padding + contentSize - titleBarHeight / 2
let buttonRadius: CGFloat = 18
let buttonSpacing: CGFloat = 56
let buttonStartX = padding + 70

// Red button
NSColor(red: 1.0, green: 0.38, blue: 0.36, alpha: 1.0).setFill()
NSBezierPath(ovalIn: NSRect(x: buttonStartX - buttonRadius, y: buttonY - buttonRadius, width: buttonRadius * 2, height: buttonRadius * 2)).fill()

// Yellow button
NSColor(red: 1.0, green: 0.74, blue: 0.21, alpha: 1.0).setFill()
NSBezierPath(ovalIn: NSRect(x: buttonStartX + buttonSpacing - buttonRadius, y: buttonY - buttonRadius, width: buttonRadius * 2, height: buttonRadius * 2)).fill()

// Green button
NSColor(red: 0.18, green: 0.82, blue: 0.35, alpha: 1.0).setFill()
NSBezierPath(ovalIn: NSRect(x: buttonStartX + buttonSpacing * 2 - buttonRadius, y: buttonY - buttonRadius, width: buttonRadius * 2, height: buttonRadius * 2)).fill()

// Content area offset
let contentX = padding + 80
let contentY = padding + 60

// Git branch icon (simplified) - orange
let branchColor = NSColor(red: 0.95, green: 0.55, blue: 0.30, alpha: 1.0)
branchColor.setStroke()
branchColor.setFill()

// Main branch line
let branchPath = NSBezierPath()
branchPath.lineWidth = 28
branchPath.lineCapStyle = .round
branchPath.move(to: NSPoint(x: contentX + 180, y: contentY + 80))
branchPath.line(to: NSPoint(x: contentX + 180, y: contentY + 480))
branchPath.stroke()

// Feature branch line
let featurePath = NSBezierPath()
featurePath.lineWidth = 28
featurePath.lineCapStyle = .round
featurePath.move(to: NSPoint(x: contentX + 180, y: contentY + 300))
featurePath.curve(to: NSPoint(x: contentX + 380, y: contentY + 450),
                   controlPoint1: NSPoint(x: contentX + 260, y: contentY + 300),
                   controlPoint2: NSPoint(x: contentX + 380, y: contentY + 370))
featurePath.stroke()

// Commit dots
let dotRadius: CGFloat = 32
NSBezierPath(ovalIn: NSRect(x: contentX + 180 - dotRadius, y: contentY + 80 - dotRadius, width: dotRadius * 2, height: dotRadius * 2)).fill()
NSBezierPath(ovalIn: NSRect(x: contentX + 180 - dotRadius, y: contentY + 300 - dotRadius, width: dotRadius * 2, height: dotRadius * 2)).fill()
NSBezierPath(ovalIn: NSRect(x: contentX + 180 - dotRadius, y: contentY + 480 - dotRadius, width: dotRadius * 2, height: dotRadius * 2)).fill()
NSBezierPath(ovalIn: NSRect(x: contentX + 380 - dotRadius, y: contentY + 450 - dotRadius, width: dotRadius * 2, height: dotRadius * 2)).fill()

// Terminal prompt ">" on the right side - green
let promptColor = NSColor(red: 0.40, green: 0.85, blue: 0.55, alpha: 1.0)
promptColor.setStroke()
let promptPath = NSBezierPath()
promptPath.lineWidth = 32
promptPath.lineCapStyle = .round
promptPath.lineJoinStyle = .round
promptPath.move(to: NSPoint(x: contentX + 480, y: contentY + 340))
promptPath.line(to: NSPoint(x: contentX + 580, y: contentY + 240))
promptPath.line(to: NSPoint(x: contentX + 480, y: contentY + 140))
promptPath.stroke()

context.restoreGState()

// Subtle border on the squircle
NSColor(white: 0.4, alpha: 0.3).setStroke()
iconPath.lineWidth = 2
iconPath.stroke()

image.unlockFocus()

// Save as PNG
if let tiffData = image.tiffRepresentation,
   let bitmap = NSBitmapImageRep(data: tiffData),
   let pngData = bitmap.representation(using: .png, properties: [:]) {
    let outputPath = (CommandLine.arguments.count > 1) ? CommandLine.arguments[1] : "icon.png"
    try! pngData.write(to: URL(fileURLWithPath: outputPath))
    print("Icon saved to \(outputPath)")
}
