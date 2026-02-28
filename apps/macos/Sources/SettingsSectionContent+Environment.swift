import SwiftUI

extension SettingsSectionContent {
    var environmentPane: some View {
        Group {
            Section("Environment Variables") {
                Text(
                    "Environment variables are injected into sandbox command execution. "
                        + "Values are write-only and never displayed."
                )
                .font(.caption)
                .foregroundStyle(.secondary)

                environmentVaultStatusMessage

                if settings.envVars.isEmpty {
                    Text("No environment variables set.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(settings.envVars) { item in
                        if settings.updatingEnvVarId == item.id {
                            HStack(spacing: 8) {
                                Text(item.key)
                                    .font(.system(.caption, design: .monospaced))
                                environmentBadge(encrypted: item.encrypted)
                                SecureField("New value", text: $settings.updatingEnvValue)
                                    .textFieldStyle(.roundedBorder)
                                    .frame(minWidth: 220)
                                    .onSubmit {
                                        settings.confirmEnvironmentVariableUpdate(key: item.key)
                                    }
                                Button("Save") {
                                    settings.confirmEnvironmentVariableUpdate(key: item.key)
                                }
                                .disabled(settings.environmentBusy)
                                Button("Cancel") {
                                    settings.cancelEnvironmentVariableUpdate()
                                }
                                .disabled(settings.environmentBusy)
                            }
                        } else {
                            HStack(spacing: 8) {
                                VStack(alignment: .leading, spacing: 2) {
                                    HStack(spacing: 6) {
                                        Text(item.key)
                                            .font(.system(.caption, design: .monospaced))
                                        environmentBadge(encrypted: item.encrypted)
                                    }
                                    HStack(spacing: 8) {
                                        Text("********")
                                        Text(item.updatedAt)
                                    }
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("Update") {
                                    settings.startEnvironmentVariableUpdate(id: item.id)
                                }
                                .disabled(settings.environmentBusy)
                                Button("Delete", role: .destructive) {
                                    settings.deleteEnvironmentVariable(id: item.id)
                                }
                                .disabled(settings.environmentBusy)
                            }
                        }
                    }
                }

                VStack(alignment: .leading, spacing: 8) {
                    Text("Add Variable")
                        .font(.subheadline)
                    LabeledContent("Key") {
                        TextField("ENV_NAME", text: $settings.newEnvKey)
                            .font(.system(.body, design: .monospaced))
                            .textFieldStyle(.roundedBorder)
                            .frame(minWidth: 300)
                            .onSubmit {
                                settings.addEnvironmentVariable()
                            }
                            .accessibilityIdentifier("settings-env-add-key")
                    }
                    LabeledContent("Value") {
                        SecureField("Type secret value", text: $settings.newEnvValue)
                            .textFieldStyle(.roundedBorder)
                            .frame(minWidth: 300)
                            .onSubmit {
                                settings.addEnvironmentVariable()
                            }
                            .accessibilityIdentifier("settings-env-add-value")
                    }
                    HStack {
                        Spacer()
                        Button(settings.environmentBusy ? "Saving..." : "Add") {
                            settings.addEnvironmentVariable()
                        }
                        .disabled(settings.environmentBusy)
                        .accessibilityIdentifier("settings-env-add-button")
                    }
                    if let message = settings.envMessage {
                        Text(message)
                            .font(.caption)
                            .foregroundStyle(.green)
                            .accessibilityIdentifier("settings-env-message")
                    }
                    if let error = settings.envError {
                        Text(error)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .accessibilityIdentifier("settings-env-error")
                    }
                }
            }

            Section("Paths") {
                LabeledContent("Config directory") {
                    Text(settings.environmentConfigDir)
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
                LabeledContent("Data directory") {
                    Text(settings.environmentDataDir)
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

extension SettingsSectionContent {
    @ViewBuilder
    var environmentVaultStatusMessage: some View {
        switch settings.environmentVaultStatus {
        case "unsealed":
            Text("Vault unlocked. Your keys are stored encrypted.")
                .font(.caption)
                .foregroundStyle(.green)
        case "sealed":
            Text(
                "Vault locked. Encrypted keys cannot be read until you unlock encryption in Security settings."
            )
            .font(.caption)
            .foregroundStyle(.orange)
        case "uninitialized":
            Text("Vault not set up. Set a password in Security to encrypt stored keys.")
                .font(.caption)
                .foregroundStyle(.secondary)
        default:
            EmptyView()
        }
    }

    func environmentBadge(encrypted: Bool) -> some View {
        Text(encrypted ? "Encrypted" : "Plaintext")
            .font(.caption2)
            .foregroundStyle(encrypted ? .green : .secondary)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(.quaternary)
            .clipShape(Capsule())
    }
}
