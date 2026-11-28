use crate::{
    app::{DotDir, Modal, read_password},
    archive::Location,
    filesystems::{ProviderPath, credential_namespace},
    ui::text,
};
use anyhow::{Result, ensure};
use dotdir_remote::{Profile, Protocol};
use gpui_kit::{
    component::input::{Input, InputState},
    prelude::FluentBuilder,
    *,
};

pub(crate) struct ConnectionForm {
    original: Option<Profile>,
    protocol: Protocol,
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    root: Entity<InputState>,
    user: Entity<InputState>,
    share: Entity<InputState>,
    region: Entity<InputState>,
    aws_profile: Entity<InputState>,
    password: Entity<InputState>,
}

impl ConnectionForm {
    fn new(
        original: Option<Profile>,
        protocol: Protocol,
        window: &mut Window,
        cx: &mut Context<DotDir>,
    ) -> Self {
        let mut field =
            |value: &str| cx.new(|cx| InputState::new(window, cx).default_value(value.to_owned()));
        let profile = original.as_ref();
        let name = field(profile.map_or("", |p| &p.name));
        let endpoint = field(profile.map_or("", |p| &p.endpoint));
        let root = field(profile.map_or("", |p| &p.root));
        let user = field(profile.map_or("", |p| &p.user));
        let share = field(profile.map_or("", |p| &p.share));
        let region = field(profile.map_or("", |p| &p.region));
        let aws_profile = field(profile.map_or("", |p| &p.aws_profile));
        Self {
            original,
            protocol,
            name,
            endpoint,
            root,
            user,
            share,
            region,
            aws_profile,
            password: cx.new(|cx| InputState::new(window, cx).masked(true)),
        }
    }

    pub fn title(&self) -> &'static str {
        if self.original.is_some() {
            "Edit Connection"
        } else {
            "New Connection"
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let input = if self.original.is_some() {
            &self.endpoint
        } else {
            &self.name
        };
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn profile(&self, cx: &App, new_secret: bool) -> Result<Profile> {
        let value = |field: &Entity<InputState>| field.read(cx).value().trim().to_owned();
        let profile = Profile {
            protocol: self.protocol,
            name: value(&self.name),
            endpoint: value(&self.endpoint),
            root: value(&self.root),
            user: value(&self.user),
            share: if matches!(self.protocol, Protocol::S3 | Protocol::Smb) {
                value(&self.share)
            } else {
                String::new()
            },
            region: if self.protocol == Protocol::S3 {
                value(&self.region)
            } else {
                String::new()
            },
            aws_profile: if self.protocol == Protocol::S3 {
                value(&self.aws_profile)
            } else {
                String::new()
            },
            credential_id: if new_secret {
                Some(Profile::new_credential_id())
            } else {
                self.original
                    .as_ref()
                    .and_then(|old| old.credential_id.clone())
            },
        };
        if let Some(old) = &self.original
            && old.credential_id.is_some()
            && !new_secret
        {
            ensure!(
                profile.endpoint == old.endpoint
                    && profile.user == old.user
                    && profile.share == old.share,
                "Re-enter the credential to change the server, share, or account"
            );
        }
        profile.validate()?;
        Ok(profile)
    }
}

fn location(profile: &Profile) -> Location {
    Location::Provider(ProviderPath::root(profile.protocol.scheme()).child(profile.name.clone()))
}

impl DotDir {
    pub(crate) fn open_remote_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_modal(
            Modal::RemoteConnections {
                form: None,
                busy: false,
            },
            window,
            cx,
        );
    }

    fn edit_remote_connection(
        &mut self,
        original: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let protocol = original
            .as_ref()
            .map(|profile| profile.protocol)
            .or_else(|| {
                Protocol::ALL
                    .into_iter()
                    .find(|protocol| protocol.enabled())
            });
        if let Some(protocol) = protocol {
            let form = ConnectionForm::new(original, protocol, window, cx);
            self.show_modal(
                Modal::RemoteConnections {
                    form: Some(Box::new(form)),
                    busy: false,
                },
                window,
                cx,
            );
        }
    }

    fn connect_remote(&mut self, profile: &Profile, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.open_panel_location(self.active, location(profile), false, window, cx);
    }

    pub(crate) fn save_remote_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::RemoteConnections {
            form: Some(form),
            busy,
        }) = &mut self.modal
        else {
            return;
        };
        if *busy {
            return;
        }
        let secret = read_password(form.password.read(cx));
        let new_credential = !secret.is_empty();
        let original = form.original.clone();
        let profile = match form.profile(cx, !secret.is_empty()).and_then(|profile| {
            ensure!(
                form.original.is_some()
                    || !self
                        .settings
                        .remote_connections
                        .iter()
                        .any(|saved| saved.protocol == profile.protocol
                            && saved.name == profile.name),
                "A connection with that name already exists"
            );
            Ok(profile)
        }) {
            Ok(profile) => profile,
            Err(error) => {
                self.report_error("Remote Connection", &error, cx);
                return;
            }
        };

        *busy = true;
        let namespace = credential_namespace(&self.settings_path);
        cx.spawn_in(window, async move |this, cx| {
            let (profile, result) = cx
                .background_executor()
                .spawn(async move {
                    let result = if secret.is_empty() {
                        Ok(())
                    } else {
                        std::str::from_utf8(&secret)
                            .map_err(anyhow::Error::from)
                            .and_then(|secret| profile.save_secret(&namespace, secret))
                    };
                    (profile, result)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(Modal::RemoteConnections { busy, .. }) = &mut this.modal {
                    *busy = false;
                }
                let saved_credential = new_credential && result.is_ok();
                let result = result.and_then(|_| {
                    let mut settings = this.settings.clone();
                    settings.remote_connections.retain(|saved| {
                        saved.protocol != profile.protocol || saved.name != profile.name
                    });
                    settings.remote_connections.push(profile.clone());
                    settings.save(&this.settings_path)?;
                    this.settings = settings;
                    this.refresh_remote_connections();
                    Ok(())
                });
                match result {
                    Ok(()) => {
                        if let Some(old) = original
                            && new_credential
                        {
                            this.remove_remote_credential(old, cx);
                        }
                        this.connect_remote(&profile, window, cx);
                    }
                    Err(error) => {
                        if saved_credential {
                            this.remove_remote_credential(profile, cx);
                        }
                        this.report_error("Could Not Save Connection", &error, cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_remote_connection(
        &mut self,
        profile: Profile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::RemoteConnections { busy, .. }) = &mut self.modal else {
            return;
        };
        if *busy {
            return;
        }
        let mut settings = self.settings.clone();
        settings
            .remote_connections
            .retain(|saved| saved.protocol != profile.protocol || saved.name != profile.name);
        if let Err(error) = settings.save(&self.settings_path) {
            self.report_error("Could Not Remove Connection", &error, cx);
            return;
        }
        self.settings = settings;
        self.refresh_remote_connections();
        self.open_remote_connections(window, cx);
        self.remove_remote_credential(profile, cx);
    }

    fn remove_remote_credential(&self, profile: Profile, cx: &mut Context<Self>) {
        if profile.credential_id.is_some() {
            let namespace = credential_namespace(&self.settings_path);
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { profile.remove_secret(&namespace) })
                    .await;
                if let Err(error) = result {
                    let _ = this.update(cx, |this, cx| {
                        this.report_error("Could Not Remove Saved Credential", &error, cx);
                    });
                }
            })
            .detach();
        }
    }

    pub(crate) fn render_remote_connections(
        &self,
        form: Option<&ConnectionForm>,
        busy: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let form_layout = || div().flex().flex_col().gap(px(10.));
        let Some(form) = form else {
            let mut view = form_layout();
            for (index, profile) in self
                .settings
                .remote_connections
                .iter()
                .filter(|profile| profile.protocol.enabled())
                .enumerate()
            {
                let connect = profile.clone();
                let edit = profile.clone();
                view = view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(text(location(profile).display())),
                        )
                        .child(self.button(
                            ("connect-remote", index),
                            "Connect",
                            move |this, window, cx| {
                                this.connect_remote(&connect, window, cx);
                            },
                            cx,
                        ))
                        .child(self.button(
                            ("edit-remote", index),
                            "Edit…",
                            move |this, window, cx| {
                                this.edit_remote_connection(Some(edit.clone()), window, cx);
                            },
                            cx,
                        )),
                );
            }
            return view
                .child(self.button(
                    "new-remote",
                    "New Connection…",
                    |this, window, cx| {
                        this.edit_remote_connection(None, window, cx);
                    },
                    cx,
                ))
                .into_any_element();
        };

        let p = self.palette;
        let protocol = form.protocol;
        let mut view = form_layout();
        if form.original.is_none() {
            let mut choices = div().flex().flex_wrap().gap(px(8.));
            for option in Protocol::ALL.into_iter().filter(|option| option.enabled()) {
                choices = choices.child(
                    self.button(
                        ("remote-protocol", option as usize),
                        option.label(),
                        move |this, _, cx| {
                            if let Some(Modal::RemoteConnections {
                                form: Some(form),
                                busy: false,
                            }) = &mut this.modal
                            {
                                form.protocol = option;
                                cx.notify();
                            }
                        },
                        cx,
                    )
                    .when(protocol == option, |button| button.border_color(p.accent)),
                );
            }
            view = view.child(choices);
        }
        let field = |label: &'static str, state: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(text(label).text_color(p.muted))
                .child(Input::new(state).aria_label(label).disabled(busy))
        };
        if let Some(old) = &form.original {
            view = view.child(text(location(old).display()).text_color(p.muted));
        } else {
            view = view.child(field("Connection name", &form.name));
        }
        let endpoint_label = match protocol {
            Protocol::Ftp => "Server URL (ftps:// or ftp://)",
            Protocol::WebDav => "Server URL (https://)",
            Protocol::Smb => "Server URL (smb://)",
            Protocol::S3 => "S3 endpoint (optional for AWS)",
        };
        view = view.child(field(endpoint_label, &form.endpoint));
        if protocol == Protocol::Smb {
            view = view.child(field("Share", &form.share));
        } else if protocol == Protocol::S3 {
            view = view.child(field("Bucket", &form.share)).child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(field("Region", &form.region)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(field("AWS profile (optional)", &form.aws_profile)),
                    ),
            );
        }
        view = view.child(field("Root folder (optional)", &form.root));
        view = view.child(
            div()
                .flex()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(field(
                    if protocol == Protocol::S3 {
                        "Access key ID"
                    } else {
                        "Username"
                    },
                    &form.user,
                )))
                .child(
                    div().flex_1().min_w_0().child(field(
                        if form
                            .original
                            .as_ref()
                            .is_some_and(|old| old.credential_id.is_some())
                        {
                            "New credential (optional)"
                        } else if protocol == Protocol::S3 {
                            "Secret access key"
                        } else {
                            "Password"
                        },
                        &form.password,
                    )),
                ),
        );
        let mut buttons = div().flex().items_center().justify_end().gap(px(8.));
        if let Some(original) = &form.original {
            let profile = original.clone();
            buttons = buttons.child(div().mr_auto().child(self.button(
                "remove-remote",
                "Remove Connection",
                move |this, window, cx| {
                    this.remove_remote_connection(profile.clone(), window, cx);
                },
                cx,
            )));
        }
        if busy {
            buttons = buttons.child(text("Saving…").text_color(p.muted));
        } else {
            buttons = buttons
                .child(self.button(
                    "back-remote",
                    "Back",
                    |this, window, cx| this.open_remote_connections(window, cx),
                    cx,
                ))
                .child(
                    self.button(
                        "save-remote",
                        "Save and Connect",
                        |this, window, cx| this.save_remote_connection(window, cx),
                        cx,
                    )
                    .bg(p.accent)
                    .text_color(p.accent_fg),
                );
        }
        view.child(buttons).into_any_element()
    }
}
