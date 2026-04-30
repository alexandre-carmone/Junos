//! Desktop tab bar — vertical strip of all tab icons on the right edge.
//!
//! Replaces `TabWheel` at the `md` breakpoint (≥768px) so desktop users see
//! every tab at once and can click directly. Mobile keeps the wheel.

use leptos::prelude::*;
use web_sys::MouseEvent;

use crate::components::tab_wheel::{tab_title, TABS};
use crate::components::tab_wheel_icons::tab_icon;
use crate::i18n::{t, Lang};
use crate::{ActiveTabCtx, Tab};

#[component]
pub fn TabBar() -> impl IntoView {
    let active = use_context::<ActiveTabCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(Tab::Sky));
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    view! {
        <div class="hidden md:flex flex-col gap-2 fixed right-2 top-1/2 -translate-y-1/2 z-[60] pointer-events-auto">
            {TABS.iter().map(|&tab| {
                view! {
                    <button
                        class=move || {
                            let base = "w-[48px] h-[48px] rounded-lg p-0 \
                                        flex items-center justify-center cursor-pointer \
                                        backdrop-blur-glass \
                                        transition-[background,border-color,box-shadow] duration-150";
                            if active.get() == tab {
                                format!("{base} bg-accent-blue-active border border-text-blue-bright text-text-dim shadow-[0_0_14px_color-mix(in_srgb,var(--text-blue)_45%,transparent)]")
                            } else {
                                format!("{base} bg-bg-wheel border border-border-strong text-text-blue shadow-none hover:border-accent-cyan")
                            }
                        }
                        title=move || tab_title(tab, &tr())
                        on:click=move |ev: MouseEvent| {
                            ev.stop_propagation();
                            active.set(tab);
                        }
                    >
                        <span
                            class=move || {
                                let s = if active.get() == tab { "w-[70%] h-[70%]" } else { "w-[58%] h-[58%]" };
                                format!("inline-block pointer-events-none {s}")
                            }
                            inner_html=tab_icon(tab)
                        />
                    </button>
                }
            }).collect_view()}

            <button
                class="mt-1 self-center min-w-[36px] h-[24px] min-h-[24px] px-2 \
                       rounded-[12px] border border-text-blue bg-bg-wheel text-text-blue \
                       font-mono font-semibold text-xs leading-none tracking-[0.05em] \
                       cursor-pointer"
                title=move || lang.get().toggle().label()
                on:click=move |ev: MouseEvent| {
                    ev.stop_propagation();
                    lang.update(|l| *l = l.toggle());
                }
            >
                {move || lang.get().label()}
            </button>
        </div>
    }
}
