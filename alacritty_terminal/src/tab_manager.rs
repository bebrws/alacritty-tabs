use crate::tty::EventedPty;
use crate::term::Term;
use crate::sync::FairMutex;
use crate::event::OnResize;
use std::sync::Arc;

use crate::event::EventListener;

// EventListener + Send + 'static

// pub trait TabManager<T: EventListener, I: tty::Pty + OnResize> {
pub trait TabManager<I: EventListener, P: EventedPty + OnResize + Send + 'static> {
// pub trait TabManager<I: EventListener> {
    // fn send_event(&self, event: Event);
    fn get_all(&self) -> &Vec<(P, Arc<FairMutex<Term<I>>>)>;
    fn current_pty(&self) -> &P;
    fn current_term(&self) -> &Arc<FairMutex<Term<I>>>;
    fn create_tab(&self);
}
